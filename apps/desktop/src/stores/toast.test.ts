import { afterEach, describe, expect, it, vi } from "vitest";
import { toastStore, showToast, dismissToast, clearToasts } from "./toast";

afterEach(() => {
  clearToasts();
  vi.useRealTimers();
});

describe("toast store", () => {
  it("adds a toast with the given message + kind", () => {
    const id = showToast("boom", "error", 0);
    const t = toastStore.toasts.find((x) => x.id === id);
    expect(t).toBeTruthy();
    expect(t!.message).toBe("boom");
    expect(t!.kind).toBe("error");
  });

  it("defaults to the error kind", () => {
    const id = showToast("oops", undefined, 0);
    expect(toastStore.toasts.find((x) => x.id === id)!.kind).toBe("error");
  });

  it("dismiss removes a toast by id", () => {
    const id = showToast("x", "info", 0);
    dismissToast(id);
    expect(toastStore.toasts.some((x) => x.id === id)).toBe(false);
  });

  it("auto-dismisses after the ttl", () => {
    vi.useFakeTimers();
    const id = showToast("temp", "info", 1000);
    expect(toastStore.toasts.some((x) => x.id === id)).toBe(true);
    vi.advanceTimersByTime(1001);
    expect(toastStore.toasts.some((x) => x.id === id)).toBe(false);
  });
});
