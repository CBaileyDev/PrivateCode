// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { addPendingPermission, permissionStore, replyPermission } from "./permissions";

type Call = { cmd: string; args: any };
let calls: Call[] = [];

beforeEach(() => {
  calls = [];
  mockIPC((cmd, args) => {
    calls.push({ cmd, args });
    return null;
  });
  addPendingPermission({
    permissionId: "p1",
    toolName: "write_file",
    action: "write",
    resources: ["/f"],
    preview: "",
    sessionId: "s1",
  });
});

afterEach(() => clearMocks());

describe("replyPermission deny-feedback", () => {
  it("forwards the feedback on reject and clears the pending slot", async () => {
    await replyPermission("reject", "no thanks");
    const c = calls.find((c) => c.cmd === "reply_permission");
    expect(c?.args).toMatchObject({
      sessionId: "s1",
      permissionId: "p1",
      reply: "reject",
      feedback: "no thanks",
    });
    expect(permissionStore.pending).toBeNull();
  });

  it("sends null feedback on allow", async () => {
    await replyPermission("once");
    const c = calls.find((c) => c.cmd === "reply_permission");
    expect(c?.args.feedback).toBeNull();
  });

  it("ignores whitespace-only feedback on reject", async () => {
    await replyPermission("reject", "   ");
    const c = calls.find((c) => c.cmd === "reply_permission");
    expect(c?.args.feedback).toBeNull();
  });
});
