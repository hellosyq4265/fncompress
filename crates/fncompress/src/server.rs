//! `fncompress serve`: opens the database, runs the encoder trial, starts
//! the scheduler, the directory monitor, the retention janitor and the HTTP
//! API on one port, and shuts down cleanly on
//! SIGINT/SIGTERM (running tasks are cancelled and go back to the queue).

use std::{future::IntoFuture, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use fncompress_core::caps;
use tokio::sync::watch;

use crate::{
    api::{self, ApiState, SystemInfo},
    app::App,
    db::{now, Db},
    events::Events,
    janitor, monitor,
    scheduler::Scheduler,
    web::{self, Assets},
};

/// How long running tasks get to stop FFmpeg and clean up on shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

pub struct ServeOptions {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    /// Tool locations from the command line or environment; stored into the
    /// global settings at startup.
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
    pub render_device: Option<String>,
    /// FFmpeg with libvmaf for 样片推荐.
    pub scoring_ffmpeg: Option<PathBuf>,
    /// dovi_tool for Dolby Vision routes.
    pub dovi_tool: Option<PathBuf>,
    /// The scoring FFmpeg shipped with the fpk (ADR 0012): becomes the
    /// scoring FFmpeg setting when that is empty and the file exists.
    pub bundled_scoring_ffmpeg: Option<PathBuf>,
    /// Also serve on this Unix socket (the fnOS unified gateway, ADR 0012).
    pub socket: Option<PathBuf>,
    /// The authorized directories file (browse roots, ADR 0012).
    pub roots_file: Option<PathBuf>,
    /// Run the trial encode of every encoder (skipping it disables the
    /// availability check).
    pub probe_encoders: bool,
}

/// Database, scheduler and API state, without the listener.
pub struct Server {
    pub app: Arc<App>,
    pub state: ApiState,
    shutdown: watch::Sender<bool>,
}

impl Server {
    pub async fn start(opts: &ServeOptions) -> Result<Self> {
        std::fs::create_dir_all(&opts.data_dir)
            .with_context(|| format!("创建数据目录 {}", opts.data_dir.display()))?;
        let db_path = opts.data_dir.join("fncompress.db");
        let db = Arc::new(
            Db::open(&db_path).with_context(|| format!("打开数据库 {}", db_path.display()))?,
        );

        let mut global = db.global_settings()?;
        if let Some(p) = &opts.ffmpeg {
            global.ffmpeg = p.clone();
        }
        if let Some(p) = &opts.ffprobe {
            global.ffprobe = p.clone();
        }
        if let Some(d) = &opts.render_device {
            global.render_device = d.clone();
        }
        if let Some(p) = &opts.scoring_ffmpeg {
            global.scoring_ffmpeg = Some(p.clone());
        }
        if let Some(p) = &opts.dovi_tool {
            global.dovi_tool = Some(p.clone());
        }
        if global.scoring_ffmpeg.is_none() {
            if let Some(p) = opts.bundled_scoring_ffmpeg.as_ref().filter(|p| p.is_file()) {
                tracing::info!(ffmpeg = %p.display(), "scoring FFmpeg from the package");
                global.scoring_ffmpeg = Some(p.clone());
            }
        }
        db.set_global_settings(&global)?;

        let requeued = db.requeue_running()?;
        if requeued > 0 {
            tracing::info!(requeued, "interrupted tasks back in the queue");
        }

        let encoders = if opts.probe_encoders {
            let (ffmpeg, device) = (global.ffmpeg.clone(), global.render_device.clone());
            tokio::task::spawn_blocking(move || caps::probe_all(&ffmpeg, &device)).await?
        } else {
            Vec::new()
        };
        for e in &encoders {
            tracing::info!(
                encoder = e.encoder.ffmpeg_name(),
                available = e.available,
                "encoder trial"
            );
        }
        let available = opts.probe_encoders.then(|| {
            encoders
                .iter()
                .filter(|e| e.available)
                .map(|e| e.encoder)
                .collect()
        });

        // Recommendation scratch space; whatever a crash left is stale.
        let work_dir = opts.data_dir.join("recommend");
        let _ = std::fs::remove_dir_all(&work_dir);

        let events = Events::default();
        let scheduler = Scheduler::new(db.clone(), events.clone(), available, work_dir);
        let app = Arc::new(App::new(db, events, scheduler.clone(), encoders));
        {
            let app = app.clone();
            let scorer = tokio::task::spawn_blocking(move || app.scorer_status()).await?;
            match scorer {
                Ok(s) if s.available => {
                    tracing::info!(ffmpeg = %s.ffmpeg.display(), version = ?s.version, "scorer ready")
                }
                Ok(s) => tracing::warn!(
                    ffmpeg = %s.ffmpeg.display(),
                    "样片推荐 unavailable: {}",
                    s.error.unwrap_or_default()
                ),
                Err(e) => tracing::warn!("scorer check failed: {e}"),
            }
        }
        let (shutdown, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move { scheduler.run().await });
        tokio::spawn(monitor::run(
            app.clone(),
            app.monitor.clone(),
            shutdown_rx.clone(),
        ));
        tokio::spawn(janitor::run(app.clone(), shutdown_rx.clone()));

        let state = ApiState {
            app: app.clone(),
            info: Arc::new(SystemInfo {
                version: env!("CARGO_PKG_VERSION").into(),
                data_dir: opts.data_dir.clone(),
                started_at: now(),
            }),
            shutdown: shutdown_rx,
            assets: Arc::new(web::Embedded),
            roots_file: opts.roots_file.clone(),
        };
        Ok(Self {
            app,
            state,
            shutdown,
        })
    }

    pub fn router(&self) -> axum::Router {
        api::router(self.state.clone())
    }

    /// The router with other front-end files (tests).
    pub fn router_with_assets(&self, assets: Arc<dyn Assets>) -> axum::Router {
        api::router(ApiState {
            assets,
            ..self.state.clone()
        })
    }

    /// Ends SSE streams, cancels running tasks and waits for them.
    pub async fn stop(&self) {
        let _ = self.shutdown.send(true);
        self.app.scheduler.shutdown(SHUTDOWN_GRACE).await;
    }
}

pub async fn serve(opts: ServeOptions) -> Result<()> {
    let server = Server::start(&opts).await?;
    let listener = tokio::net::TcpListener::bind(opts.listen)
        .await
        .with_context(|| format!("监听 {}", opts.listen))?;
    tracing::info!(listen = %opts.listen, data_dir = %opts.data_dir.display(), "fncompress serving");

    let (stop_tx, stop_rx) = watch::channel(false);
    let stopped = move || {
        let mut rx = stop_rx.clone();
        async move {
            let _ = rx.wait_for(|s| *s).await;
        }
    };
    let mut servers = vec![tokio::spawn(
        axum::serve(listener, server.router())
            .with_graceful_shutdown(stopped())
            .into_future(),
    )];
    if let Some(path) = &opts.socket {
        servers.push(serve_socket(
            path,
            api::admin_only(server.router()),
            stopped(),
        )?);
    }

    shutdown_signal().await;
    tracing::info!("shutting down");
    // SSE streams end first so the HTTP server can drain.
    server.stop().await;
    let _ = stop_tx.send(true);
    let drained = futures_util::future::join_all(servers);
    match tokio::time::timeout(Duration::from_secs(10), drained).await {
        Ok(joined) => {
            for j in joined {
                j??;
            }
        }
        Err(_) => tracing::warn!("HTTP connections still open after 10 s"),
    }
    if let Some(path) = &opts.socket {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

type Served = tokio::task::JoinHandle<std::io::Result<()>>;

/// The router on a Unix socket, for the fnOS gateway. The socket is 0666
/// because the gateway's nginx runs as www-data; requests need
/// `X-Trim-Isadmin: true` (`api::admin_only`).
#[cfg(unix)]
fn serve_socket(
    path: &std::path::Path,
    router: axum::Router,
    stopped: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<Served> {
    use std::os::unix::fs::PermissionsExt;
    // A socket left by a crash blocks the bind.
    let _ = std::fs::remove_file(path);
    let listener =
        tokio::net::UnixListener::bind(path).with_context(|| format!("监听 {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
        .with_context(|| format!("设置 {} 的权限", path.display()))?;
    tracing::info!(socket = %path.display(), "fncompress serving");
    Ok(tokio::spawn(
        axum::serve(listener, router)
            .with_graceful_shutdown(stopped)
            .into_future(),
    ))
}

#[cfg(not(unix))]
fn serve_socket(
    path: &std::path::Path,
    _router: axum::Router,
    _stopped: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<Served> {
    anyhow::bail!("{}：只有 Unix 支持 socket 监听", path.display())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::warn!("no SIGTERM handler: {e}");
                std::future::pending::<()>().await
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}
