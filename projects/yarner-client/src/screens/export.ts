// Export: a passphrase-protected bundle of the story, downloaded as `<title>.yarner`.

import type { ExportReport } from "../api.ts";
import type { Source } from "../source.ts";
import { action, bind, h, modal, signal } from "../ui.ts";

export function openExport(src: Source, title: string) {
  const ed = src.edit;
  if (!ed) return;
  const pass = signal("");
  const confirm = signal("");
  const report = signal<ExportReport | null>(null);
  const problem = () =>
    pass().length < 8
      ? "Use at least 8 characters."
      : pass() !== confirm()
        ? "The passphrases don't match."
        : null;

  modal("Export", () =>
    h(
      "div",
      { class: "export" },
      h(
        "p",
        { class: "muted" },
        "A bundle is one encrypted file anyone with its passphrase can open (and import into their own yarner). Choose a passphrase for it; it can differ from this story's.",
      ),
      h(
        "form",
        { class: "form", onSubmit: (e: Event) => e.preventDefault() },
        h(
          "label",
          null,
          "Bundle passphrase",
          h("input", {
            type: "password",
            name: "export-passphrase",
            autocomplete: "new-password",
            ...bind(pass),
          }),
        ),
        h(
          "label",
          null,
          "Confirm",
          h("input", {
            type: "password",
            name: "export-confirm",
            autocomplete: "new-password",
            ...bind(confirm),
          }),
        ),
        () =>
          pass() || confirm() ? h("p", { class: "muted small" }, problem() ?? "Looks good.") : null,
        action(
          "Export",
          async () => {
            const { blob, report: r } = await ed.export(pass());
            report.set(r);
            download(blob, `${safeName(title)}.yarner`);
          },
          { class: "primary", type: "submit", disabled: () => !!problem() },
        ),
      ),
      () => {
        const r = report();
        if (!r) return null;
        return h(
          "div",
          { class: "report" },
          h("h3", null, "Exported"),
          h("p", null, `${r.beats} beats.`),
          r.pending_dropped
            ? h(
                "p",
                null,
                `${r.pending_dropped} beats written ahead but never reached were left out.`,
              )
            : null,
          r.media_unfinished
            ? h(
                "p",
                null,
                `${r.media_unfinished} pictures or videos still being made were left out.`,
              )
            : null,
          r.untaken.length
            ? h(
                "div",
                null,
                h(
                  "p",
                  null,
                  `${r.untaken.length} choices nobody took. They'll be shown disabled in a read-only player:`,
                ),
                h(
                  "ul",
                  { class: "untaken" },
                  r.untaken.slice(0, 50).map(([, label]) => h("li", null, label)),
                ),
                r.untaken.length > 50
                  ? h("p", { class: "muted small" }, `…and ${r.untaken.length - 50} more.`)
                  : null,
              )
            : null,
        );
      },
    ),
  );
}

function safeName(s: string): string {
  return (s.trim() || "story").replace(/[\\/:*?"<>|]+/g, "-").slice(0, 80);
}

function download(blob: Blob, name: string) {
  const url = URL.createObjectURL(blob);
  const a = h("a", { href: url, download: name });
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 30_000);
}
