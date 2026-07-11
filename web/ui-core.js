(function (root, factory) {
  var api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  root.SubagentUI = api;
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  "use strict";

  function escapeHtml(value) {
    return String(value == null ? "" : value).replace(
      /[&<>"']/g,
      function (character) {
        return {
          "&": "&amp;",
          "<": "&lt;",
          ">": "&gt;",
          '"': "&quot;",
          "'": "&#39;",
        }[character];
      },
    );
  }

  function humanTime(value) {
    if (!value) return "—";
    var date = new Date(value);
    return Number.isNaN(date.getTime()) ? String(value) : date.toLocaleString();
  }

  function patchLineKind(line) {
    if (/^\*\*\* (Begin|End) Patch$/.test(line)) return "boundary";
    if (/^\*\*\* (Add|Update|Delete) File:/.test(line)) return "file";
    if (/^@@/.test(line)) return "hunk";
    if (/^\+/.test(line)) return "addition";
    if (/^-/.test(line)) return "deletion";
    return "context";
  }

  function patchDiffHtml(patch) {
    var oldLine = 0;
    var newLine = 0;
    var lines = String(patch || "")
      .replace(/\r\n/g, "\n")
      .split("\n");
    return (
      '<div class="diff" role="region" aria-label="Patch diff">' +
      lines
        .map(function (line) {
          var kind = patchLineKind(line);
          var oldLabel = "";
          var newLabel = "";
          if (kind === "file" || kind === "hunk") {
            oldLine = 0;
            newLine = 0;
          } else if (kind === "addition") {
            newLine += 1;
            newLabel = newLine;
          } else if (kind === "deletion") {
            oldLine += 1;
            oldLabel = oldLine;
          } else if (kind === "context" && line && !line.startsWith("***")) {
            oldLine += 1;
            newLine += 1;
            oldLabel = oldLine;
            newLabel = newLine;
          }
          return (
            '<div class="diff-line diff-' +
            kind +
            '"><span class="diff-old">' +
            oldLabel +
            '</span><span class="diff-new">' +
            newLabel +
            "</span><code>" +
            escapeHtml(line || " ") +
            "</code></div>"
          );
        })
        .join("") +
      "</div>"
    );
  }

  function parseArguments(data) {
    if (data && data.patch_preview != null)
      return { patch: data.patch_preview };
    try {
      return JSON.parse((data && data.arguments) || "{}");
    } catch (_) {
      return null;
    }
  }

  function valueHtml(value, depth) {
    depth = depth || 0;
    if (value == null) return '<span class="empty-value">none</span>';
    if (typeof value === "boolean")
      return '<span class="value-chip">' + (value ? "yes" : "no") + "</span>";
    if (typeof value === "number")
      return '<span class="number-value">' + value + "</span>";
    if (typeof value === "string")
      return '<pre class="plain-output">' + escapeHtml(value) + "</pre>";
    if (Array.isArray(value)) {
      if (!value.length) return '<span class="empty-value">none</span>';
      return (
        '<ul class="value-list">' +
        value
          .map(function (entry) {
            return "<li>" + valueHtml(entry, depth + 1) + "</li>";
          })
          .join("") +
        "</ul>"
      );
    }
    if (depth > 3) return '<span class="empty-value">nested details</span>';
    return (
      '<dl class="field-list">' +
      Object.keys(value)
        .filter(function (key) {
          return (
            value[key] != null && key !== "head_bytes" && key !== "tail_bytes"
          );
        })
        .map(function (key) {
          return (
            "<div><dt>" +
            escapeHtml(key.replaceAll("_", " ")) +
            "</dt><dd>" +
            valueHtml(value[key], depth + 1) +
            "</dd></div>"
          );
        })
        .join("") +
      "</dl>"
    );
  }

  function toolCallHtml(data) {
    var name = (data && data.name) || "tool";
    var args = parseArguments(data);
    if (name === "apply_patch" && args && typeof args.patch === "string") {
      return (
        '<div class="tool-description">Applying workspace changes</div>' +
        patchDiffHtml(args.patch) +
        (data.preview_truncated
          ? '<button class="load-full-patch" type="button">Load complete diff</button>'
          : "")
      );
    }
    if (!args)
      return '<p class="minor">Arguments are too large for the preview. Open full details to inspect them.</p>';
    var prominent = args.command || args.path || args.pattern || args.query;
    return (
      (prominent
        ? '<pre class="command-block">' + escapeHtml(prominent) + "</pre>"
        : "") +
      valueHtml(
        Object.fromEntries(
          Object.entries(args).filter(function (entry) {
            return !["command", "path", "pattern", "query"].includes(entry[0]);
          }),
        ),
      )
    );
  }

  function toolResultHtml(data) {
    var result =
      data && data.result
        ? data.result
        : data && data.summary
          ? data.summary
          : {};
    var output =
      result && result.output && result.output.content != null
        ? result.output.content
        : result.output_preview;
    var status =
      result.ok === false
        ? "failed"
        : result.status || (result.ok === true ? "completed" : "result");
    var html =
      '<div class="result-summary"><span class="result-status ' +
      (result.ok === false ? "bad" : "good") +
      '">' +
      escapeHtml(status) +
      "</span>";
    if (result.exit_code != null)
      html += "<span>exit " + escapeHtml(result.exit_code) + "</span>";
    if (result.path) html += "<span>" + escapeHtml(result.path) + "</span>";
    if (result.bytes != null)
      html += "<span>" + escapeHtml(result.bytes) + " bytes</span>";
    html += "</div>";
    if (output)
      html += '<pre class="terminal-output">' + escapeHtml(output) + "</pre>";
    if (data && data.result)
      html += valueHtml(
        Object.fromEntries(
          Object.entries(result).filter(function (entry) {
            return ![
              "output",
              "output_preview",
              "ok",
              "status",
              "exit_code",
              "path",
              "bytes",
            ].includes(entry[0]);
          }),
        ),
      );
    return html;
  }

  function eventBodyHtml(event) {
    var data = event.data || {};
    if (event.type === "tool_call") return toolCallHtml(data);
    if (event.type === "tool_result") return toolResultHtml(data);
    if (
      [
        "system_message",
        "user_message",
        "assistant_message",
        "reasoning",
      ].includes(event.type)
    ) {
      return (
        '<div class="message-content">' +
        escapeHtml(data.content || data.text || "") +
        "</div>" +
        (data.preview_truncated
          ? '<div class="minor">Preview truncated</div>'
          : "")
      );
    }
    if (event.type === "error")
      return (
        '<div class="error-content">' +
        escapeHtml(data.error || data.message || "Agent error") +
        "</div>"
      );
    if (event.type === "lifecycle") {
      return (
        '<div class="lifecycle-content"><span class="status">' +
        escapeHtml(data.status || "working") +
        "</span>" +
        (data.reason
          ? "<span>" + escapeHtml(data.reason.replaceAll("_", " ")) + "</span>"
          : "") +
        "</div>"
      );
    }
    return valueHtml(data);
  }

  function noticeText(value) {
    if (typeof value === "string") return value;
    if (!value) return "Done";
    if (value.message) return value.message;
    if (value.type === "message_sent") return "Message queued";
    if (value.type === "side_created") return "Side run started";
    if (value.type === "side_deleted") return "Side history deleted";
    if (value.type === "agent_renamed") return "Agent renamed to " + value.name;
    if (value.type === "agent_deleted") return "Agent deleted";
    if (value.status) return String(value.status).replaceAll("_", " ");
    return "Done";
  }

  function isNearBottom(scrollHeight, scrollTop, clientHeight, threshold) {
    return scrollHeight - scrollTop - clientHeight <= (threshold || 120);
  }

  function anchoredScrollTop(oldTop, oldHeight, newHeight) {
    return oldTop + Math.max(0, newHeight - oldHeight);
  }

  return {
    escapeHtml: escapeHtml,
    humanTime: humanTime,
    patchLineKind: patchLineKind,
    patchDiffHtml: patchDiffHtml,
    parseArguments: parseArguments,
    valueHtml: valueHtml,
    toolCallHtml: toolCallHtml,
    toolResultHtml: toolResultHtml,
    eventBodyHtml: eventBodyHtml,
    noticeText: noticeText,
    isNearBottom: isNearBottom,
    anchoredScrollTop: anchoredScrollTop,
  };
});
