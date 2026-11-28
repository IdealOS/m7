// The Sound section's choices: outputs and levelling.

import assert from "node:assert/strict";
import { test } from "node:test";

import { AUTOMATIC, chosenOutput, levelingFrom, levelingLabel, levelingOf, LEVELINGS, outputChoices } from "../src/sound.ts";

const outputs = [
  { id: "sysdefault:CARD=Headphones", name: "Headphones" },
  { id: "hdmi:CARD=vc4hdmi0,DEV=0", name: "HDMI 1" },
  { id: "plughw:CARD=A,DEV=0", name: "USB Audio" },
  { id: "plughw:CARD=B,DEV=0", name: "USB Audio" },
];

test("the outputs offered: Automatic, then each there is, told apart", () => {
  assert.deepEqual(
    outputChoices(outputs, null).map((c) => [c.label, c.id]),
    [
      [AUTOMATIC, null],
      ["Headphones", "sysdefault:CARD=Headphones"],
      ["HDMI 1", "hdmi:CARD=vc4hdmi0,DEV=0"],
      ["USB Audio (plughw:CARD=A,DEV=0)", "plughw:CARD=A,DEV=0"],
      ["USB Audio (plughw:CARD=B,DEV=0)", "plughw:CARD=B,DEV=0"],
    ],
  );
});

test("a chosen output that's unplugged stays on the list", () => {
  const choices = outputChoices(outputs.slice(0, 2), "plughw:CARD=DAC,DEV=0");
  assert.deepEqual(choices.at(-1), { label: "plughw:CARD=DAC,DEV=0 (not connected)", id: "plughw:CARD=DAC,DEV=0" });
  assert.equal(outputChoices(outputs, "hdmi:CARD=vc4hdmi0,DEV=0").length, 5, "one that's there isn't listed twice");
});

test("the output setting's value", () => {
  assert.equal(chosenOutput("hdmi:CARD=vc4hdmi0,DEV=0"), "hdmi:CARD=vc4hdmi0,DEV=0");
  assert.equal(chosenOutput(null), null);
  assert.equal(chosenOutput(""), null);
  assert.equal(chosenOutput(3), null);
});

test("levelling, read as audiod reads it", () => {
  assert.equal(levelingOf(undefined), "track", "unset: each song alike");
  assert.equal(levelingOf("album"), "album");
  assert.equal(levelingOf("loud"), "track");
  for (const [l, name] of LEVELINGS) {
    assert.equal(levelingLabel(l), name);
    assert.equal(levelingFrom(name), l);
  }
  assert.equal(levelingFrom("nonsense"), undefined);
});
