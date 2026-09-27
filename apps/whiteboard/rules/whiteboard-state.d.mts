export type Note = {
  id: string;
  text: string;
  x: number;
  y: number;
  color: string;
  locked: boolean;
  revision: number;
};
export type NoteCommandPayload = {
  component_key: string;
  kind: string;
  visibility: string;
  text: string;
  color: string;
  locked: boolean;
  position_x: number;
  position_y: number;
};
export type WhiteboardState = {
  notes: Map<string, any>;
  pendingCommands: Map<string, any>;
};
export const NOTE_COMPONENT: string;
export function notePayload(note: Omit<Note, "revision"> & { revision: number | bigint }, changes?: Partial<Omit<Note, "revision">>): NoteCommandPayload;
export function createWhiteboardState(): WhiteboardState;
export function applyEntity(state: WhiteboardState, entity: unknown): boolean;
export function applyEntityCommand(state: WhiteboardState, command: any): boolean;
