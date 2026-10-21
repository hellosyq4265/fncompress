import { onUnmounted, ref } from 'vue'

/** Phone layout below this width (decisions doc: phones only view and cancel tasks). */
export const MOBILE_MAX = 767

export function useIsMobile() {
  const query = window.matchMedia(`(max-width: ${MOBILE_MAX}px)`)
  const mobile = ref(query.matches)
  const update = (e: MediaQueryListEvent) => (mobile.value = e.matches)
  query.addEventListener('change', update)
  onUnmounted(() => query.removeEventListener('change', update))
  return mobile
}
