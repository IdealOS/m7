// Permissions, through the `perm` service. An app starts with none: it may use
// the documents it creates, and asks for more with `request`, which shows the
// user a prompt. Apps usually list what they need in their package.json's
// `idealos.permissions` instead, which the system grants when it installs them.

import { request as busRequest } from "./bus.ts";
import type { Grant, GrantsResult, RequestResult, Rule } from "./protocol.ts";

export type { Grant, Rule } from "./protocol.ts";

/** How long a prompt may wait for the user (as long as permd waits). */
const PROMPT_TIMEOUT_MS = 10 * 60 * 1000;

/**
 * Ask for `rules`, with `reason` shown to the user in your own words. Resolves
 * true once the app has them all (the user allowed it, or it had them already),
 * false if the user said no.
 */
export async function request(rules: Rule[], reason?: string): Promise<boolean> {
  const params = reason === undefined ? { rules } : { rules, reason };
  const r = await busRequest<RequestResult>("perm", "request", params, { timeoutMs: PROMPT_TIMEOUT_MS });
  return r.granted;
}

/** The app's own grants. */
export async function granted(): Promise<Grant[]> {
  return (await busRequest<GrantsResult>("perm", "grants", {})).grants;
}
