/**
 * Usage store — tracks token counts and cost for the active session.
 * Updated reactively as usage_updated events arrive from the engine.
 */
import { createStore } from "solid-js/store";

export interface UsageData {
  input_tokens: number;
  output_tokens: number;
  reasoning_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  cost: number;
}

interface UsageState {
  session: UsageData;
  turn: UsageData;
  model: string;
  provider: string;
}

const emptyUsage: UsageData = {
  input_tokens: 0,
  output_tokens: 0,
  reasoning_tokens: 0,
  cache_read_tokens: 0,
  cache_write_tokens: 0,
  cost: 0,
};

const [usageStore, setUsageStore] = createStore<UsageState>({
  session: { ...emptyUsage },
  turn: { ...emptyUsage },
  model: "claude-opus-4-8",
  provider: "anthropic",
});

function updateUsage(usage: UsageData) {
  setUsageStore("session", { ...usage });
}

function updateTurnUsage(usage: UsageData) {
  setUsageStore("turn", { ...usage });
}

function resetTurnUsage() {
  setUsageStore("turn", { ...emptyUsage });
}

function setModelInfo(model: string, provider: string) {
  setUsageStore("model", model);
  setUsageStore("provider", provider);
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}

function formatCost(cost: number): string {
  if (cost < 0.01) return `$${cost.toFixed(4)}`;
  return `$${cost.toFixed(2)}`;
}

export {
  usageStore,
  setUsageStore,
  updateUsage,
  updateTurnUsage,
  resetTurnUsage,
  setModelInfo,
  formatTokens,
  formatCost,
};
