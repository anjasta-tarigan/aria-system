// The Extensions modal: built-in addons for Aria.
//
// Sections: Skills, Plugins, Connectors (MCPs).
//
// This UI is mock-backed for now: it lists items, lets the user enable /
// disable them, and shows toasts. Real backend wiring (skill/plugin
// registration, OpenAPI connector ingestion, MCP stdio servers) comes later
// once the UI shape is proven.

import { focusables, h } from "./dom";
import type { Ipc } from "./app";

const STR = {
  title: "Extensions",
  close: "Close extensions",
  sections: {
    skill: "Skills",
    plugin: "Plugins",
    connector: "Connectors / MCP",
  },
  skills: {
    intro:
      "Prompt packages that teach Aria new behaviors. A skill bundles a system prompt with the tools it is allowed to use.",
    none: "No skills installed yet.",
  },
  plugins: {
    intro:
      "JavaScript/TypeScript extensions that run inside Aria's process. Placeholder: no plugin host is wired yet.",
    none: "No plugins installed.",
  },
  connectors: {
    intro:
      "Connections to external services (MCP servers, OpenAPI specs). Placeholder: connector ingestion comes next.",
    none: "No connectors configured.",
  },
  toggleFailed: (kind: string) => `Could not update ${kind}.`,
  toggled: (name: string, enabled: boolean) =>
    `${name} is now ${enabled ? "enabled" : "disabled"}.`,
  enable: "Enable",
  disable: "Disable",
  enabled: "Enabled",
  disabled: "Disabled",
  add: "Add…",
  addHelp: "Coming soon: install from a URL or local file.",
};

type ExtensionKind = "skill" | "plugin" | "connector";

interface ExtensionItem {
  id: string;
  name: string;
  description: string;
  enabled: boolean;
  kind: ExtensionKind;
}

/// Mock catalog. Replaced by backend data once skills/plugins/connectors are
/// wired up on the Rust side.
const MOCK: ExtensionItem[] = [
  {
    id: "skill-summarize",
    name: "Summarize",
    description: "Condenses long threads into key decisions and next actions.",
    enabled: true,
    kind: "skill",
  },
  {
    id: "skill-review",
    name: "Code Review",
    description: "Reviews diffs for bugs, style, and security issues.",
    enabled: false,
    kind: "skill",
  },
  {
    id: "skill-meeting-notes",
    name: "Meeting Notes",
    description: "Turns a transcript into structured notes with action items.",
    enabled: true,
    kind: "skill",
  },
  {
    id: "plugin-linter",
    name: "Lint Plugin",
    description: "Runs the project linter on edited files.",
    enabled: false,
    kind: "plugin",
  },
  {
    id: "connector-github",
    name: "GitHub",
    description: "MCP connector for issues, PRs, and repositories.",
    enabled: false,
    kind: "connector",
  },
  {
    id: "connector-slack",
    name: "Slack",
    description: "MCP connector for channels and messages.",
    enabled: false,
    kind: "connector",
  },
];

/**
 * Open the Extensions modal. `ipc` is the app's invoke adapter so the same
 * code runs against the real backend and the in-browser mock.
 */
export function openExtensions(ipc: Ipc, onClose?: () => void): void {
  // One dialog at a time.
  const prevBackdrop = document.querySelector(
    ".extensions-backdrop",
  ) as HTMLElement & { _close?: () => void } | null;
  prevBackdrop?._close?.();
  prevBackdrop?.remove();
  const returnFocus = document.activeElement as HTMLElement | null;

  let closed = false;

  const state = {
    loading: false,
    items: [...MOCK],
  };

  const overlay = h("div", {
    class: "modal-backdrop extensions-backdrop",
    onclick: (e: MouseEvent) => {
      if (e.target === overlay) void requestClose();
    },
  });
  (overlay as HTMLElement & { _close?: () => void })._close = () => {
    closed = true;
  };
  const live = h("div", {
    class: "toast-region",
    "aria-live": "polite",
    "aria-atomic": "false",
  });
  overlay.append(live);

  let dialog: HTMLElement;
  let onKey: (e: KeyboardEvent) => void;

  const close = () => {
    closed = true;
    document.removeEventListener("keydown", onKey, true);
    overlay.remove();
    returnFocus?.focus?.();
    onClose?.();
  };

  const requestClose = () => {
    close();
  };

  onKey = (e: KeyboardEvent) => {
    if (document.querySelector(".confirm-backdrop")) return;
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      void requestClose();
      return;
    }
    if (e.key !== "Tab" || !dialog) return;
    const items = focusables(dialog);
    if (items.length === 0) return;
    const first = items[0];
    const last = items[items.length - 1];
    const active = document.activeElement as HTMLElement | null;
    if (!e.shiftKey && active === last) {
      e.preventDefault();
      first.focus();
    } else if (e.shiftKey && (active === first || !dialog.contains(active))) {
      e.preventDefault();
      last.focus();
    }
  };
  document.addEventListener("keydown", onKey, true);

  const toast = (message: string, kind: "ok" | "bad" = "ok") => {
    const el = h("div", { class: `toast ${kind}`, role: "status" }, message);
    live.append(el);
    setTimeout(() => el.remove(), 4000);
  };

  // --------------------------------------------------------------- rendering

  const itemRow = (item: ExtensionItem): HTMLElement => {
    const toggle = h(
      "button",
      {
        class: `ghost small-btn ${item.enabled ? "" : "danger"}`,
        disabled: state.loading,
        onclick: async () => {
          const next = !item.enabled;
          toggle.disabled = true;
          render();
          try {
            // Backend call goes here once extensions are wired up:
            // await ipc.invoke("set_extension", { id: item.id, enabled: next });
            item.enabled = next;
            state.items = state.items.map((i) =>
              i.id === item.id ? { ...i, enabled: next } : i,
            );
            toast(STR.toggled(item.name, next));
          } catch {
            toast(STR.toggleFailed(item.kind), "bad");
            item.enabled = !next;
            state.items = state.items.map((i) =>
              i.id === item.id ? { ...i, enabled: !next } : i,
            );
          } finally {
            toggle.disabled = false;
            render();
          }
        },
      },
      item.enabled ? STR.disable : STR.enable,
    );
    return h(
      "div",
      { class: "extension-item" },
      h(
        "div",
        { class: "extension-item-main" },
        h("div", { class: "extension-item-name" }, item.name),
        h(
          "span",
          {
            class: `extension-item-badge ${item.enabled ? "on" : "off"}`,
            title: item.enabled ? STR.enabled : STR.disabled,
          },
          item.enabled ? STR.enabled : STR.disabled,
        ),
      ),
      h("p", { class: "extension-item-desc muted" }, item.description),
      toggle,
    );
  };

  const sectionPanel = (kind: ExtensionKind): HTMLElement => {
    const items = state.items.filter((i) => i.kind === kind);
    const title = STR.sections[kind];
    const intro = (() => {
      switch (kind) {
        case "skill":
          return STR.skills.intro;
        case "plugin":
          return STR.plugins.intro;
        case "connector":
          return STR.connectors.intro;
      }
    })();
    const empty = (() => {
      switch (kind) {
        case "skill":
          return STR.skills.none;
        case "plugin":
          return STR.plugins.none;
        case "connector":
          return STR.connectors.none;
      }
    })();
    return h(
      "section",
      { class: "extension-section", id: `extension-section-${kind}` },
      h("h3", { class: "settings-section-title" }, title),
      h("p", { class: "settings-intro" }, intro),
      items.length
        ? h("div", { class: "extension-list" }, ...items.map(itemRow))
        : h("div", { class: "empty-state" }, h("p", { class: "muted" }, empty)),
      h(
        "div",
        { class: "extension-add" },
        h(
          "button",
          {
            class: "ghost small-btn",
            disabled: true,
            title: STR.addHelp,
          },
          STR.add,
        ),
      ),
    );
  };

  const render = () => {
    if (closed) return;
    dialog = h(
      "div",
      {
        class: "modal extensions-modal",
        role: "dialog",
        "aria-modal": "true",
        "aria-labelledby": "extensions-title",
        tabindex: "-1",
      },
      h(
        "header",
        { class: "settings-header" },
        h("h2", { id: "extensions-title" }, STR.title),
        h(
          "button",
          {
            class: "ghost icon-btn",
            "aria-label": STR.close,
            title: STR.close,
            onclick: () => void requestClose(),
          },
          "×",
        ),
      ),
      h("div", { class: "settings-body" }, live, h("div", { class: "settings-content" }, ...(["skill", "plugin", "connector"] as ExtensionKind[]).map(sectionPanel))),
    );

    const activeId = document.activeElement?.id;
    const existing = overlay.querySelector(".extensions-modal");
    if (existing) existing.replaceWith(dialog);
    else overlay.append(dialog);
    if (!overlay.isConnected) document.body.append(overlay);

    if (activeId) {
      const target = dialog.querySelector<HTMLElement>(`#${CSS.escape(activeId)}`);
      if (target && document.activeElement !== target) target.focus();
    }
  };

  render();
}

export type { ExtensionItem, ExtensionKind };
