/** Capacity phase in an existing realtime ErrorMessage; no framing change. */
export type CheckpointCapacityPhase = "not_admitted" | "completed";

/** Only not_admitted may retry the same ID as a new admission. A completed
 * rejection replays for its original ID even if capacity later becomes free. */
export function checkpointCapacityPhase(error: {
  code: string;
  retryable: boolean;
}): CheckpointCapacityPhase | undefined {
  if (error.code !== "CHECKPOINT_CAPACITY") return undefined;
  return error.retryable ? "not_admitted" : "completed";
}
