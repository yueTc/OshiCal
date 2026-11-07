export interface TimelineDayRect {
  key: string;
  top: number;
  bottom: number;
}

export function visibleTimelineDayKeys(
  days: readonly TimelineDayRect[],
  visibleTop: number,
  visibleBottom: number,
  minimumVisiblePx = 2,
): string[] {
  if (visibleBottom <= visibleTop) return [];
  return days
    .filter((day) => Math.min(day.bottom, visibleBottom) - Math.max(day.top, visibleTop) > minimumVisiblePx)
    .map((day) => day.key);
}

export function timelineAnchorCompensation(currentTop: number, desiredTop: number): number {
  return currentTop - desiredTop;
}

export function clampTimelineScroll(value: number, maxScrollTop: number): number {
  return Math.min(Math.max(0, maxScrollTop), Math.max(0, value));
}

export function timelineDragScrollTop(
  startScrollTop: number,
  pointerDeltaY: number,
  maxScrollTop: number,
): number {
  return clampTimelineScroll(startScrollTop - pointerDeltaY, maxScrollTop);
}
