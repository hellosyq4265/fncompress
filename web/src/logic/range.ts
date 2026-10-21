// How a route plan carries the source's dynamic range: a tag and one line
// for HDR10, HDR10+, HLG and Dolby Vision; nothing for SDR.
import type { DoviConfig, RangeRoute } from '../api/types'
import type { TagType } from './format'

export interface RangeView {
  tag: string
  type: TagType
  detail: string
}

export function doviLabel(c: DoviConfig): string {
  return c.profile === 8 ? `P8.${c.bl_compat_id}` : `P${c.profile}`
}

export function rangeView(r: RangeRoute | null | undefined): RangeView | null {
  switch (r?.kind) {
    case 'hdr10':
      return { tag: 'HDR10', type: 'info', detail: '10-bit 编码，保留色彩标注和静态元数据（母版显示、内容亮度）' }
    case 'hdr10_plus':
      return {
        tag: 'HDR10+',
        type: 'info',
        detail: '10-bit 编码，保留静态元数据；逐帧写回 HDR10+ 动态元数据，校验逐帧核对',
      }
    case 'hlg':
      return { tag: 'HLG', type: 'info', detail: '10-bit 编码，保留色彩标注' }
    case 'dolby_vision': {
      const from = doviLabel(r.source)
      const to = doviLabel(r.output)
      const plus = r.hdr10_plus ? ' + HDR10+' : ''
      const also = r.hdr10_plus ? '；HDR10+ 动态元数据同样逐帧保留并核对' : ''
      if (from !== to) {
        return {
          tag: `杜比视界 ${from} → ${to}${plus}`,
          type: 'warning',
          // The plan's own warning explains what the enhancement layer loses.
          detail: `RPU 转为 ${to} 后写入成品，校验逐帧核对 RPU${also}`,
        }
      }
      return { tag: `杜比视界 ${from}${plus}`, type: 'info', detail: `保留 RPU 和配置记录，校验逐帧核对 RPU${also}` }
    }
    default:
      return null
  }
}
