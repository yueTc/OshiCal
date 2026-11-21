import assert from "node:assert/strict";
import test from "node:test";
import { resolveScheduleRoomUrl } from "../src/lib/schedule-room.ts";

const subscriber = {
  mid: "3537115310721781",
  name: "思诺snow",
  aliases: ["思诺"],
  roomId: "30858592",
  liveUrl: "https://live.bilibili.com/30858592",
};
const solo = {
  roomUrl: null,
  roomOwnerMid: null,
  roomOwnerName: null,
  participants: [subscriber.name],
};
const group = { ...solo, participants: [subscriber.name, "心宜不是心仪"] };

test("a collaboration opens its explicit room, even when the host is not subscribed", () => {
  const url = "https://live.bilibili.com/30849777";
  assert.equal(resolveScheduleRoomUrl({ ...group, roomUrl: url }, subscriber), url);
});

test("a collaboration without a room never falls back to a participant's room", () => {
  assert.equal(resolveScheduleRoomUrl(group, subscriber), null);
  assert.equal(resolveScheduleRoomUrl({ ...group, roomOwnerName: "心宜不是心仪" }, subscriber), null);
});

test("a different named or identified host also blocks fallback for a single participant", () => {
  assert.equal(resolveScheduleRoomUrl({ ...solo, roomOwnerName: "心宜不是心仪" }, subscriber), null);
  assert.equal(resolveScheduleRoomUrl({ ...solo, roomOwnerMid: "3537115310721181" }, subscriber), null);
});

test("a solo schedule can use its subscriber's room URL or actual room number, but not a UID", () => {
  assert.equal(resolveScheduleRoomUrl(solo, subscriber), subscriber.liveUrl);
  assert.equal(resolveScheduleRoomUrl({ ...solo, roomOwnerName: "思诺" }, subscriber), subscriber.liveUrl);
  assert.equal(resolveScheduleRoomUrl(solo, { ...subscriber, liveUrl: null }), subscriber.liveUrl);
  const withUid = { ...solo, roomOwnerMid: subscriber.mid };
  assert.equal(resolveScheduleRoomUrl(withUid, { ...subscriber, liveUrl: null, roomId: null }), null);
  assert.equal(resolveScheduleRoomUrl(withUid), null);
});
