// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  candidateStore,
  clearCandidates,
  handleCandidateEvent,
  selectNext,
  selectPrev,
  selectedPane,
} from "./candidates";

beforeEach(() => clearCandidates());
afterEach(() => clearCandidates());

const started = (i: number, model: string) => ({
  type: "candidate_started",
  session_id: "A",
  candidate_index: i,
  model_id: model,
});
const textDelta = (i: number, t: string) => ({
  type: "candidate_delta",
  session_id: "A",
  candidate_index: i,
  delta: { kind: "text", text: t },
});
const completed = (i: number, error: string | null = null) => ({
  type: "candidate_completed",
  session_id: "A",
  candidate_index: i,
  usage: {},
  finish_reason: error ? null : "end_turn",
  error,
});

describe("candidate comparison store", () => {
  it("assembles per-candidate panes from started + delta + completed", () => {
    handleCandidateEvent(started(0, "anthropic/opus"));
    handleCandidateEvent(started(1, "nvidia/llama"));
    handleCandidateEvent(textDelta(0, "alpha "));
    handleCandidateEvent(textDelta(0, "answer"));
    handleCandidateEvent(textDelta(1, "beta answer"));

    expect(candidateStore.panes).toHaveLength(2);
    expect(candidateStore.active).toBe(true);
    expect(candidateStore.panes[0].modelId).toBe("anthropic/opus");
    expect(candidateStore.panes[0].text).toBe("alpha answer");
    expect(candidateStore.panes[1].text).toBe("beta answer");
    expect(candidateStore.panes[0].status).toBe("streaming");

    handleCandidateEvent(completed(0));
    handleCandidateEvent(completed(1));
    expect(candidateStore.panes[0].status).toBe("done");
    expect(candidateStore.panes[0].finishReason).toBe("end_turn");
    expect(candidateStore.active).toBe(false);
  });

  it("marks a failed candidate as error and keeps the message", () => {
    handleCandidateEvent(started(0, "bad/model"));
    handleCandidateEvent(completed(0, "provider 'bad' is not registered"));
    expect(candidateStore.panes[0].status).toBe("error");
    expect(candidateStore.panes[0].error).toContain("not registered");
  });

  it("a new fan-out (candidate_index 0) resets the prior comparison", () => {
    handleCandidateEvent(started(0, "m0"));
    handleCandidateEvent(started(1, "m1"));
    handleCandidateEvent(textDelta(0, "old"));
    expect(candidateStore.panes).toHaveLength(2);

    // Next turn starts: index 0 clears the previous panes.
    handleCandidateEvent(started(0, "new0"));
    expect(candidateStore.panes).toHaveLength(1);
    expect(candidateStore.panes[0].modelId).toBe("new0");
    expect(candidateStore.panes[0].text).toBe("");
    expect(candidateStore.active).toBe(true);
  });

  it("keeps panes ordered by candidate_index regardless of arrival order", () => {
    handleCandidateEvent(started(0, "m0")); // index 0 first to begin the set
    handleCandidateEvent(started(2, "m2"));
    handleCandidateEvent(started(1, "m1"));
    expect(candidateStore.panes.map((p) => p.index)).toEqual([0, 1, 2]);
  });

  it("ignores a delta for a candidate that never started", () => {
    handleCandidateEvent(textDelta(5, "orphan"));
    expect(candidateStore.panes).toHaveLength(0);
  });

  it("keyboard selection clamps to valid panes", () => {
    handleCandidateEvent(started(0, "m0"));
    handleCandidateEvent(started(1, "m1"));
    expect(candidateStore.selected).toBe(0);
    selectPrev();
    expect(candidateStore.selected).toBe(0); // clamped low
    selectNext();
    expect(candidateStore.selected).toBe(1);
    selectNext();
    expect(candidateStore.selected).toBe(1); // clamped high
    expect(selectedPane()?.modelId).toBe("m1");
  });
});
