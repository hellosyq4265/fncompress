// Minimal but schema-complete objects for tests.
import type { Candidate, Report, Task } from '../api/types'

const identity = { dev: 1, ino: 2, size: 1000, mtime_ns: 3 }

export function task(id: number, over: Partial<Task> = {}): Task {
  return {
    id,
    type: 'transcode',
    source: 'manual',
    path: `/vol1/Movies/film${id}.mkv`,
    state: 'queued',
    quota: 'gpu',
    force: false,
    profile_id: 'default',
    fingerprint: 'abc',
    snapshot: {} as Task['snapshot'],
    observe: false,
    created_at: 1_700_000_000 + id,
    ...over,
  }
}

export function report(findings: Report['findings'] = []): Report {
  const verdict = findings.some((f) => f.severity === 'hard')
    ? 'rejected'
    : findings.some((f) => f.severity === 'soft')
      ? 'passed_with_warnings'
      : 'passed'
  return { verdict, findings, inspection: { errors: [] } }
}

export function candidate(r: Report = report(), saved_pct = 40): Candidate {
  return {
    source: '/vol1/a.mkv',
    path: '/vol1/.a.fncompress-1-x.mkv',
    target: '/vol1/a.mkv',
    source_identity: identity,
    identity,
    savings: { source_size: 1000, candidate_size: 1000 * (1 - saved_pct / 100), saved_pct },
    report: r,
    plan: {
      encoder: 'hevc_qsv',
      video_index: 0,
      container: 'mkv',
      target: '/vol1/a.mkv',
      pix_fmt: 'nv12',
      full_range: false,
      width: 1920,
      height: 1080,
      container_changed: false,
      expected_losses: [],
      warnings: [],
    },
  }
}
