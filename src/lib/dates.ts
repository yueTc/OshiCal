import {
  addDays,
  format,
  isSameDay,
  startOfDay,
  startOfWeek,
} from "date-fns";
import { zhCN } from "date-fns/locale";

export function todayStart(): Date {
  return startOfDay(new Date());
}

export function weekStart(date: Date): Date {
  return startOfWeek(date, { weekStartsOn: 1 });
}

export function rollingDays(anchor: Date): Date[] {
  return Array.from({ length: 7 }, (_, index) => addDays(startOfDay(anchor), index));
}

export function dayLabel(date: Date): string {
  return format(date, "M月d日 EEEE", { locale: zhCN });
}

export function shortDayLabel(date: Date): string {
  return format(date, "M/d EEE", { locale: zhCN });
}

export function timeLabel(value: string): string {
  return format(new Date(value), "HH:mm");
}

export function dateTimeLabel(value: string): string {
  return format(new Date(value), "M月d日 HH:mm", { locale: zhCN });
}

export function inputDateTime(value: string): string {
  return format(new Date(value), "yyyy-MM-dd'T'HH:mm");
}

export function eventDayKey(value: string): string {
  return format(new Date(value), "yyyy-MM-dd");
}

export function localDateKey(date: Date): string {
  return format(date, "yyyy-MM-dd");
}

export function rangeAroundDays(anchor: Date, daysBefore: number, daysAfter: number): { from: string; to: string } {
  const from = addDays(startOfDay(anchor), -daysBefore);
  const to = addDays(startOfDay(anchor), daysAfter + 1);
  return { from: from.toISOString(), to: to.toISOString() };
}

export { addDays, format, isSameDay };

