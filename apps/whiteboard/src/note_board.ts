export type Note = { id: string; text: string; x: number; y: number; color: string; locked: boolean; revision: bigint };

type NoteView = {
  element: HTMLElement;
  textarea: HTMLTextAreaElement;
  icon: HTMLElement;
  lock: HTMLButtonElement;
  remove: HTMLButtonElement;
  status: HTMLElement;
  dirty: boolean;
};

type Actions = {
  text: (id: string, text: string) => boolean;
  lock: (id: string) => void;
  remove: (id: string) => void;
  drag: (event: PointerEvent, element: HTMLElement, id: string) => void;
};

/** Keep each editor mounted while canonical state changes around it. */
export class NoteBoard {
  private readonly views = new Map<string, NoteView>();

  constructor(private readonly board: HTMLElement, private readonly actions: Actions) {}

  render(notes: Iterable<Note>, joined: boolean, pending: (id: string) => boolean): void {
    const retained = new Set<string>();
    for (const note of notes) {
      retained.add(note.id);
      let view = this.views.get(note.id);
      if (!view) {
        view = this.create(note.id);
        this.views.set(note.id, view);
        this.board.appendChild(view.element);
      }
      view.element.className = `note ${note.color} ${note.locked ? "locked" : ""}`;
      view.element.style.left = `${note.x}px`;
      view.element.style.top = `${note.y}px`;
      // Assigning textarea.value resets selection, even when re-rendering is
      // otherwise harmless. Unsubmitted input also survives temporary disable.
      if (!view.dirty && this.board.ownerDocument.activeElement !== view.textarea && view.textarea.value !== note.text) {
        view.textarea.value = note.text;
      }
      const disabled = !joined || pending(note.id);
      view.textarea.disabled = disabled;
      view.lock.disabled = disabled;
      view.remove.disabled = disabled;
      view.icon.textContent = note.locked ? "🔒" : "✦";
      view.lock.textContent = note.locked ? "解除" : "ロック";
      view.lock.setAttribute("aria-label", note.locked ? "ロック解除" : "ロック");
      view.status.textContent = note.locked ? "ロック中" : "未ロック";
    }
    for (const [id, view] of this.views) {
      if (retained.has(id)) continue;
      view.element.remove();
      this.views.delete(id);
    }
  }

  private create(id: string): NoteView {
    const document = this.board.ownerDocument;
    const element = document.createElement("article");
    element.dataset.noteId = id;
    const head = document.createElement("div");
    head.className = "note-head";
    const icon = document.createElement("span"), buttons = document.createElement("span");
    const lock = document.createElement("button"), remove = document.createElement("button");
    lock.className = "lock-button";
    remove.textContent = "×";
    remove.setAttribute("aria-label", "削除");
    buttons.append(lock, remove);
    head.append(icon, buttons);
    const textarea = document.createElement("textarea"), status = document.createElement("small");
    textarea.setAttribute("aria-label", "付箋本文");
    element.append(head, textarea, status);
    const view: NoteView = { element, textarea, icon, lock, remove, status, dirty: false };
    textarea.addEventListener("input", () => { view.dirty = true; });
    textarea.addEventListener("change", () => {
      if (this.actions.text(id, textarea.value)) view.dirty = false;
    });
    lock.addEventListener("click", () => this.actions.lock(id));
    remove.addEventListener("click", () => this.actions.remove(id));
    element.addEventListener("pointerdown", event => this.actions.drag(event, element, id));
    return view;
  }
}
