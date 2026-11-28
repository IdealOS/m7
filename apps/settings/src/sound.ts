// The Sound section's choices, apart from the widgets so they can be tested:
// which output to play to, and how to level loudness. Both are Setting
// documents audiod follows (`audio.output`, `audio.leveling`).

import type { Leveling, OutputDevice } from "@idealos/sdk";

/** The choice that leaves the output to the system. */
export const AUTOMATIC = "Automatic";

/** An output to offer: what the menu says, and the setting's value for it. */
export interface OutputChoice {
  label: string;
  id: string | null;
}

/**
 * The outputs to choose from: Automatic, then each there is now. Two with one
 * name are told apart by their ids. A chosen output that isn't there now (a USB
 * DAC unplugged, say) stays on the list, so choosing again isn't the only way
 * to see what's set.
 */
export function outputChoices(outputs: OutputDevice[], chosen: string | null): OutputChoice[] {
  const named = (o: OutputDevice) => (outputs.filter((x) => x.name === o.name).length > 1 ? `${o.name} (${o.id})` : o.name);
  const choices: OutputChoice[] = [{ label: AUTOMATIC, id: null }, ...outputs.map((o) => ({ label: named(o), id: o.id }))];
  if (chosen !== null && !outputs.some((o) => o.id === chosen)) choices.push({ label: `${chosen} (not connected)`, id: chosen });
  return choices;
}

/** The `audio.output` setting's value: an output's id, or null for Automatic. */
export function chosenOutput(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

export const LEVELINGS: [Leveling, string][] = [
  ["off", "Off"],
  ["track", "Each song alike"],
  ["album", "Each album alike"],
];

/** The `audio.leveling` setting's value, as audiod reads it (each song alike when unset). */
export function levelingOf(value: unknown): Leveling {
  return LEVELINGS.find(([l]) => l === value)?.[0] ?? "track";
}

export function levelingLabel(l: Leveling): string {
  return LEVELINGS.find(([x]) => x === l)?.[1] ?? "";
}

export function levelingFrom(label: string): Leveling | undefined {
  return LEVELINGS.find(([, name]) => name === label)?.[0];
}
