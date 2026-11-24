import type { EnvironmentId } from "@t3tools/contracts";

/** The upstream prompt asks to star the upstream project; this fork asks nothing. */
export async function recordPromptSent(_environmentId: EnvironmentId): Promise<void> {}
