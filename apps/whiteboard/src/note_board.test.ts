import { it } from "node:test";
import assert from "node:assert/strict";
import { NoteBoard, type Note } from "./note_board.js";

// A small DOM surface with browser-relevant editor behavior: replacing/moving
// nodes loses focus, and assigning value resets the selection. The production
// renderer itself runs here; no second rendering implementation is under test.
class Element extends EventTarget {
  children: Element[] = [];
  parent: Element | null = null;
  dataset: Record<string, string> = {};
  style: Record<string, string> = {};
  attributes: Record<string, string> = {};
  className = "";
  textContent = "";
  disabled = false;
  selectionStart = 0;
  selectionEnd = 0;
  private text = "";
  constructor(readonly ownerDocument: DocumentStub, readonly tag: string) { super(); }
  get value() { return this.text; }
  set value(text: string) { this.text = text; this.selectionStart = this.selectionEnd = text.length; }
  setAttribute(name: string, value: string) { this.attributes[name] = value; }
  append(...elements: Element[]) { for (const element of elements) this.appendChild(element); }
  appendChild(element: Element) {
    if (element.parent) element.remove();
    this.children.push(element); element.parent = this;
    return element;
  }
  remove() {
    if (this.contains(this.ownerDocument.activeElement)) this.ownerDocument.activeElement = null;
    if (this.parent) this.parent.children = this.parent.children.filter(element => element !== this);
    this.parent = null;
  }
  contains(element: Element | null): boolean {
    return element === this || this.children.some(child => child.contains(element));
  }
  focus() { this.ownerDocument.activeElement = this; }
}
class DocumentStub {
  activeElement: Element | null = null;
  createElement(tag: string) { return new Element(this, tag); }
}

const note = (id: string, text = id): Note => ({ id, text, x: 10, y: 20, color: "yellow", locked: false, revision: 1n });
function setup() {
  const document = new DocumentStub(), board = document.createElement("section");
  const submitted: Array<[string, string]> = [];
  let accept = true;
  const view = new NoteBoard(board as unknown as HTMLElement, {
    text: (id, text) => { if (accept) submitted.push([id, text]); return accept; },
    lock: () => {}, remove: () => {}, drag: () => {},
  });
  const textarea = (id: string) => board.children.find(element => element.dataset.noteId === id)!.children.find(element => element.tag === "textarea")!;
  return { document, board, view, submitted, textarea, refuse: () => { accept = false; } };
}

it("another note's update preserves the mounted editor, draft, focus and selection", () => {
  const { document, board, view, textarea } = setup();
  view.render([note("a"), note("b")], true, () => false);
  const editor = textarea("a"), article = board.children[0];
  editor.focus(); editor.value = "入力中の下書き";
  editor.selectionStart = 2; editor.selectionEnd = 5;
  editor.dispatchEvent(new Event("input"));
  view.render([note("a"), { ...note("b", "他の人の変更"), x: 90, revision: 2n }], true, () => false);
  assert.equal(board.children[0], article);
  assert.equal(textarea("a"), editor);
  assert.equal(editor.value, "入力中の下書き");
  assert.equal(document.activeElement, editor);
  assert.deepEqual([editor.selectionStart, editor.selectionEnd], [2, 5]);
  assert.equal(textarea("b").value, "他の人の変更");
});

it("a same-note update retains the draft until change; confirmation then restores canonical text", () => {
  const { document, view, textarea, submitted } = setup();
  view.render([note("a", "initial")], true, () => false);
  const editor = textarea("a"); editor.focus(); editor.value = "my draft";
  editor.dispatchEvent(new Event("input"));
  view.render([note("a", "remote edit")], true, () => false);
  assert.equal(editor.value, "my draft");
  document.activeElement = null;
  editor.dispatchEvent(new Event("change"));
  assert.deepEqual(submitted, [["a", "my draft"]]);
  view.render([note("a", "confirmed text")], true, () => false);
  assert.equal(editor.value, "confirmed text");
});

it("drafts survive a temporary disconnect and are not submitted while unavailable", () => {
  const { view, textarea, submitted, refuse, document } = setup();
  view.render([note("a")], true, () => false);
  const editor = textarea("a"); editor.value = "not submitted";
  editor.dispatchEvent(new Event("input"));
  view.render([note("a")], false, () => false);
  refuse(); document.activeElement = null;
  editor.dispatchEvent(new Event("change"));
  view.render([note("a", "new snapshot")], true, () => false);
  assert.equal(editor.value, "not submitted");
  assert.equal(editor.disabled, false);
  assert.deepEqual(submitted, []);
});

it("repeated renders do not duplicate handlers or disturb focus when a different note is deleted", () => {
  const { view, textarea, submitted, document, board } = setup();
  for (let i = 0; i < 5; i++) view.render([note("a"), note("b")], true, () => false);
  const editor = textarea("a"); editor.focus();
  view.render([note("a")], true, () => false);
  assert.equal(document.activeElement, editor);
  editor.value = "one operation"; editor.dispatchEvent(new Event("input")); editor.dispatchEvent(new Event("change"));
  assert.deepEqual(submitted, [["a", "one operation"]]);
  view.render([], true, () => false);
  assert.equal(board.children.length, 0);
  assert.equal(document.activeElement, null);
});
