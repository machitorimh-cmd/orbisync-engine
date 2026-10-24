export type Page<T> = { items: T[]; next_cursor?: string | null };
export class CursorPages<T extends { id: string }> {
  constructor(maxItems?: number);
  readonly maxItems: number;
  readonly items: T[];
  readonly nextCursor: string | null;
  readonly busy: boolean;
  readonly started: boolean;
  reset(): void;
  upsert(item: T): void;
  load(fetchPage: (cursor: string | null) => Promise<Page<T>>): Promise<boolean>;
}
export function pagePath(resource: "worlds" | "instances", cursor?: string | null): string;
