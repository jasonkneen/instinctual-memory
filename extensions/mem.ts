// installed by `mem setup`; managed by mem.
// Reinstall or `mem setup pi|omp --remove` overwrites/removes this file.
//
// Durable project memory for the pi and omp harnesses (omp is a pi fork):
//   - at session start, the user's working preferences and a pointer to the
//     project's memory are appended to the system prompt;
//   - on every prompt, the curated facts that match it are appended;
//   - at session end, the finished session is backfilled in the background;
//   - memory_search / memory_read / memory_history / memory_status /
//     memory_request_change become native tools, so the agent does not have
//     to ask or load a skill first.
//
// The native tools are skipped when a `mem` MCP server is configured (omp),
// so the model always sees exactly one set of memory tools. The extension
// tells the two harnesses apart by the directory it was installed into, so
// the pi copy always provides the tools. Override with MEM_EXTENSION_TOOLS=1
// (force tools) or MEM_EXTENSION_TOOLS=0 (force skip).
//
// Set MEM_BIN to point at a `mem` binary that is not on PATH.

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { StringEnum } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const MEM_BIN = "__MEM_BIN__";
const TIMEOUT_MS = 20_000;

/** Absolute path of this extension file, or "" if it cannot be resolved. */
const EXTENSION_PATH = (() => {
  try {
    return fileURLToPath(import.meta.url);
  } catch {
    return "";
  }
})();

/** True when this copy was installed for omp (its path is under `~/.omp`). */
const IS_OMP = /[/\\]\.omp[/\\]/.test(EXTENSION_PATH);

/** True when an MCP `mem` server is already configured for this harness. */
function mcpMemConfigured(): boolean {
  if (process.env.MEM_EXTENSION_TOOLS === "1") return false;
  if (process.env.MEM_EXTENSION_TOOLS === "0") return true;
  // pi has no MCP support: its copy always exposes the native tools.
  if (!IS_OMP) return false;
  const candidates = [
    join(homedir(), ".omp", "agent", "mcp.json"),
    join(process.cwd(), ".omp", "mcp.json"),
    join(process.cwd(), ".mcp.json"),
  ];
  for (const path of candidates) {
    try {
      const cfg = JSON.parse(readFileSync(path, "utf8"));
      if (cfg?.mcpServers?.mem) return true;
    } catch {
      // Missing or unreadable config: keep looking.
    }
  }
  return false;
}

const REGISTER_TOOLS = !mcpMemConfigured();

/** Run the `mem` binary and parse its JSON stdout, or fail. */
async function runOp(
  pi: ExtensionAPI,
  operation: string,
  args: unknown,
  cwd: string,
): Promise<{ payload: unknown; code: number | undefined }> {
  const res = await pi.exec(MEM_BIN, ["op", operation, "--args", JSON.stringify(args ?? {})], {
    timeout: TIMEOUT_MS,
    cwd,
  });
  const out = (res.stdout ?? "").trim();
  if (out) {
    try {
      return { payload: JSON.parse(out), code: res.code ?? undefined };
    } catch {
      return { payload: out, code: res.code ?? undefined };
    }
  }
  throw new Error((res.stderr ?? "").trim() || `mem op ${operation} failed`);
}

/** The context `mem hook` would inject, as plain text, or undefined. */
async function hookContext(
  pi: ExtensionAPI,
  event: "start" | "prompt",
  cwd: string,
  prompt?: string,
): Promise<string | undefined> {
  const args = ["hook", event, "--plain", "--cwd", cwd];
  if (prompt !== undefined) args.push("--prompt", prompt);
  try {
    const res = await pi.exec(MEM_BIN, args, { timeout: 10_000, cwd });
    const text = (res.stdout ?? "").trim();
    return text || undefined;
  } catch {
    return undefined;
  }
}

/** Backfill and (when enough built up) consolidate, without blocking exit. */
function syncInBackground(cwd: string): void {
  try {
    const child = spawn(MEM_BIN, ["hook", "sync"], { cwd, detached: true, stdio: "ignore" });
    child.unref();
  } catch {
    // Never fail a session over memory.
  }
}

export default function memExtension(pi: ExtensionAPI): void {
  // Cached session-start context; recomputed for each new session.
  let startContext: string | undefined;
  let startLoaded = false;

  async function ensureStartContext(cwd: string): Promise<string | undefined> {
    if (startLoaded) return startContext;
    startLoaded = true;
    startContext = await hookContext(pi, "start", cwd);
    return startContext;
  }

  pi.on("session_start", () => {
    startContext = undefined;
    startLoaded = false;
  });

  pi.on("before_agent_start", async (event, ctx: ExtensionContext) => {
    const [start, relevant] = await Promise.all([
      ensureStartContext(ctx.cwd),
      hookContext(pi, "prompt", ctx.cwd, event.prompt),
    ]);
    const extra = [start, relevant].filter(Boolean).join("\n");
    if (!extra) return undefined;
    return { systemPrompt: `${event.systemPrompt}\n\n${extra}` };
  });

  pi.on("session_shutdown", async (_event, ctx: ExtensionContext) => {
    syncInBackground(ctx.cwd);
  });

  if (REGISTER_TOOLS) {
    pi.registerTool({
      name: "memory_search",
      label: "Memory Search",
      description:
        "Search this project's durable memory: curated facts (fact_* ids) and raw session events (journal:* ids), reranked. Use before answering about past decisions, conventions, architecture, or the user's preferences.",
      promptSnippet: "Search durable project memory (facts and past sessions)",
      promptGuidelines: [
        "Use memory_search before answering questions about past decisions, conventions, or the user's preferences.",
      ],
      parameters: Type.Object({
        query: Type.String({ description: "What to look for, in plain words." }),
        limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 20, description: "Hits to return. Default 8." })),
        rerank: Type.Optional(Type.Boolean({ description: "Set false to keep the lexical order." })),
        project: Type.Optional(Type.String({ description: "Only this project's memory." })),
      }),
      async execute(_id, params, _signal, _onUpdate, ctx) {
        return toResult(await runOp(pi, "memory_search", params, ctx.cwd));
      },
    });

    pi.registerTool({
      name: "memory_read",
      label: "Memory Read",
      description: "Read a fact, an entity's current facts, or a journal event by the id a search returned.",
      parameters: Type.Object({
        id: Type.String({ description: "fact_*, an entity id (p_, o_, w_, pref_), or journal:<event>#<seq>." }),
      }),
      async execute(_id, params, _signal, _onUpdate, ctx) {
        return toResult(await runOp(pi, "memory_read", params, ctx.cwd));
      },
    });

    pi.registerTool({
      name: "memory_history",
      label: "Memory History",
      description: "Every fact of an entity with its status (current, superseded, retracted).",
      parameters: Type.Object({ id: Type.String({ description: "Fact id or entity id." }) }),
      async execute(_id, params, _signal, _onUpdate, ctx) {
        return toResult(await runOp(pi, "memory_history", params, ctx.cwd));
      },
    });

    pi.registerTool({
      name: "memory_status",
      label: "Memory Status",
      description:
        "Journal size and projects, consolidation lag, fact counts, pending changes, and the active reranker and extractor.",
      parameters: Type.Object({}),
      async execute(_id, _params, _signal, _onUpdate, ctx) {
        return toResult(await runOp(pi, "memory_status", {}, ctx.cwd));
      },
    });

    pi.registerTool({
      name: "memory_request_change",
      label: "Memory Change",
      description:
        "Change durable memory now: remember a fact, correct one (the old fact is superseded), or forget one (never returned again). Use when the user states a standing rule, decision, or correction worth keeping.",
      promptSnippet: "Remember, correct, or forget a durable fact",
      promptGuidelines: [
        "Use memory_request_change when the user states a new standing rule or decision, corrects a remembered fact, or asks to forget one.",
      ],
      parameters: Type.Object({
        kind: StringEnum(["remember", "correct", "forget"] as const),
        entity_id: Type.String({ description: "p_<person>, o_<org>, w_<project>, or pref_<name>." }),
        statement: Type.Optional(Type.String({ description: "The fact as one sentence (remember, correct)." })),
        fact_id: Type.Optional(Type.String({ description: "Id for the new fact (remember, correct)." })),
        target_fact_id: Type.Optional(Type.String({ description: "Fact to correct or forget." })),
        reason: Type.Optional(Type.String({ description: "Why, recorded with a forget." })),
        request_id: Type.Optional(Type.String({ description: "Idempotency key; generated when omitted." })),
      }),
      async execute(_id, params, _signal, _onUpdate, ctx) {
        const args = { ...params, request_id: params.request_id ?? `pi_${Date.now()}_${Math.random().toString(36).slice(2, 10)}` };
        return toResult(await runOp(pi, "memory_request_change", args, ctx.cwd));
      },
    });
  }

  pi.registerCommand("mem", {
    description: "Show this project's durable memory status",
    handler: async (_args, ctx) => {
      try {
        const res = await pi.exec(MEM_BIN, ["--output-format", "text", "status"], { cwd: ctx.cwd, timeout: 10_000 });
        ctx.ui.notify((res.stdout ?? "").trim() || "no mem store here (run `mem init`)", "info");
      } catch {
        ctx.ui.notify("mem is not available", "error");
      }
    },
  });
}

function toResult(result: { payload: unknown; code: number | undefined }) {
  const text = typeof result.payload === "string" ? result.payload : JSON.stringify(result.payload, null, 2);
  return { content: [{ type: "text" as const, text }], details: result.payload };
}
