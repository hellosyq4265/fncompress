// Verification reports (ADR 0004) for display: findings grouped by tier,
// check ids with Chinese names, and the FFmpeg inspection in one line each.
import type { Finding, Report, Severity, Task } from '../api/types'
import { formatDuration } from './format'

/** Check ids from crates/core/src/verify.rs (and the pipeline's notes). */
export const CHECK_LABEL: Record<string, string> = {
  probe_depth: '探测深度',
  stream_layout: '轨道布局',
  video_codec: '视频编码',
  video_size: '分辨率',
  video_format: '像素格式',
  color: '色彩描述',
  frame_count: '视频帧数',
  packet_count: '复制流包数',
  stream_codec: '复制流编码',
  stream_language: '轨道语言',
  stream_title: '轨道标题',
  stream_disposition: '轨道属性',
  decode: '完整解码',
  spot_check_ssim: '画面抽检',
  stream_hash: '复制流哈希',
  timeline: '时间线检查点',
  av_offset: '音画偏移',
  stream_end: '流结束时间',
  chapters: '章节',
  hdr_metadata: 'HDR 元数据',
  dovi_rpu: '杜比视界 RPU',
  dolby_vision: '杜比视界',
  predict_savings: '预判收益',
  marker: '已处理标记',
  stream_tags: '轨道标签',
  global_tags: '全局标签',
  inspection: '检查未能运行',
  time_base: '时间基',
  muxer_tags: '封装器标签',
  encoder_sei: '编码器 SEI',
  container_duration: '容器总时长',
}

export const checkLabel = (id: string): string => CHECK_LABEL[id] ?? id

export interface FindingGroup {
  severity: Severity
  findings: Finding[]
}

const ORDER: Severity[] = ['hard', 'soft', 'info']

/** Non-empty groups, hard first. */
export function groupFindings(findings: Finding[]): FindingGroup[] {
  return ORDER.map((severity) => ({ severity, findings: findings.filter((f) => f.severity === severity) })).filter(
    (g) => g.findings.length > 0,
  )
}

/** The report a task carries, wherever its outcome keeps it. */
export function reportOf(t: Task): Report | null {
  const o = t.outcome
  if (!o) return t.candidate?.report ?? null
  switch (o.outcome) {
    case 'skipped':
    case 'predicted_no_gain':
      return null
    case 'rejected':
    case 'verified':
      return o.candidate.report
    case 'insufficient_savings':
    case 'published':
      return o.report
  }
}

/** Lines on the FFmpeg-based checks: decode, spot checks, frame counts. */
export function inspectionLines(r: Report): string[] {
  const i = r.inspection
  const lines: string[] = []
  if (i.decode) {
    const how = i.decode.hardware ? '核显' : '软件'
    lines.push(
      i.decode.errors.length === 0
        ? `完整解码（${how}）：无错误`
        : `完整解码（${how}）：${i.decode.errors.length} 个错误`,
    )
  }
  if (i.spot_checks && i.spot_checks.length > 0) {
    const worst = i.spot_checks.reduce((a, b) => (b.ssim < a.ssim ? b : a))
    lines.push(
      `画面抽检 ${i.spot_checks.length} 处，最低 SSIM ${worst.ssim.toFixed(3)}（${formatDuration(worst.at_s)} 处）`,
    )
  }
  if (i.decoded_frames && i.decoded_frames.length === 2) {
    lines.push(`解码帧数：源片 ${i.decoded_frames[0]}，成品 ${i.decoded_frames[1]}`)
  }
  if (i.stream_hashes && i.stream_hashes.length > 0) {
    const same = i.stream_hashes.filter((h) => h.source === h.candidate).length
    lines.push(`复制流哈希：${same}/${i.stream_hashes.length} 条一致`)
  }
  for (const [check, why] of i.errors) lines.push(`${checkLabel(check)}未能运行：${why}`)
  return lines
}
