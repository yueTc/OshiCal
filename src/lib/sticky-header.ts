import { useEffect, useRef, useState } from "react";

/**
 * 跟踪吸顶标题条是否已经贴住窗口顶部。
 * 组件只有在真正吸附后才显示遮罩，避免未滚动时盖住页面顶部的横幅。
 */
export function useStickyHeader<T extends HTMLElement>() {
  const stickyRef = useRef<T>(null);
  const [stickyPinned, setStickyPinned] = useState(false);

  useEffect(() => {
    const element = stickyRef.current;
    if (!element) {
      return;
    }
    let frame = 0;
    const measure = () => {
      frame = 0;
      const pinned = element.getBoundingClientRect().top <= 0.5;
      setStickyPinned((current) => (current === pinned ? current : pinned));
    };
    const schedule = () => {
      if (!frame) {
        frame = window.requestAnimationFrame(measure);
      }
    };
    measure();
    window.addEventListener("scroll", schedule, { passive: true });
    window.addEventListener("resize", schedule);
    return () => {
      window.removeEventListener("scroll", schedule);
      window.removeEventListener("resize", schedule);
      if (frame) {
        window.cancelAnimationFrame(frame);
      }
    };
  }, []);

  return { stickyRef, stickyPinned };
}
