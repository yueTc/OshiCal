import assert from "node:assert/strict";
import test from "node:test";
import { captureMenuScroll } from "../src/lib/menu-scroll.ts";

function fixture() {
  const view = { scrollX: 0, scrollY: 0 };
  const document = { defaultView: view };
  const parent = { scrollLeft: 0, scrollTop: 1200, parentElement: null, ownerDocument: document };
  const child = { scrollLeft: 0, scrollTop: 0, parentElement: parent, ownerDocument: document };
  return { view, parent, child, changed: captureMenuScroll(child as unknown as Element) };
}

test("queued scroll notifications without displacement do not dismiss a newly opened menu", () => {
  const { changed } = fixture();
  assert.equal(changed(), false);
  assert.equal(changed(), false);
});

test("real vertical and horizontal ancestor scrolling dismisses the menu", () => {
  const vertical = fixture();
  vertical.parent.scrollTop += 1;
  assert.equal(vertical.changed(), true);
  const horizontal = fixture();
  horizontal.parent.scrollLeft += 1;
  assert.equal(horizontal.changed(), true);
});

test("window scrolling is detected even when element scroll positions do not change", () => {
  const { view, changed } = fixture();
  view.scrollY = 20;
  assert.equal(changed(), true);
});

test("a later menu opening captures the new baseline, not the previous menu's position", () => {
  const { child, parent, changed } = fixture();
  parent.scrollTop += 30;
  assert.equal(changed(), true);
  const nextMenu = captureMenuScroll(child as unknown as Element);
  assert.equal(nextMenu(), false);
  parent.scrollTop += 1;
  assert.equal(nextMenu(), true);
});
