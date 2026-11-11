/**
 * 在菜单打开的瞬间记录锚点及其祖先的实际滚动位置。
 * 浏览器可能稍后才派发此前的 scroll 事件；无位移通知不应关闭刚打开的菜单。
 */
export function captureMenuScroll(anchor: Element | null): () => boolean {
  const positions: { element: Element; left: number; top: number }[] = [];
  for (let element = anchor; element; element = element.parentElement) {
    positions.push({ element, left: element.scrollLeft, top: element.scrollTop });
  }
  const view = anchor?.ownerDocument.defaultView;
  const windowLeft = view?.scrollX;
  const windowTop = view?.scrollY;
  return () => positions.some(({ element, left, top }) => element.scrollLeft !== left || element.scrollTop !== top)
    || Boolean(view && (view.scrollX !== windowLeft || view.scrollY !== windowTop));
}
