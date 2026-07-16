(function () {
  "use strict";
  var UI = window.SubagentUI,
    selected = null,
    selectedRef = null,
    selectedSide = null,
    selectedSideRef = null,
    agentCache = new Map(),
    timelineController = null,
    inboxOffset = 0;
  var visibleTypes =
    "system_message,user_message,assistant_message,tool_call,tool_result,lifecycle,error";
  var $ = function (selector) {
    return document.querySelector(selector);
  };

  function notice(value) {
    var element = $("#notice");
    element.textContent = UI.noticeText(value);
    element.style.display = "block";
    clearTimeout(notice.timer);
    notice.timer = setTimeout(function () {
      element.style.display = "none";
    }, 4000);
  }
  async function api(path, options) {
    options = options || {};
    options.headers = Object.assign({}, options.headers || {});
    if (options.body) options.headers["Content-Type"] = "application/json";
    var response = await fetch(path, options),
      text = await response.text(),
      value = null;
    try {
      if (text) value = JSON.parse(text);
    } catch (_) {}
    if (!response.ok)
      throw new Error(
        value && value.message ? value.message : text || response.statusText,
      );
    return value;
  }
  async function lines(path) {
    var response = await fetch(path),
      text = await response.text();
    if (!response.ok) {
      var value = null;
      try {
        value = JSON.parse(text);
      } catch (_) {}
      throw new Error(
        value && value.message ? value.message : response.statusText,
      );
    }
    return text.trim() ? text.trim().split("\n").map(JSON.parse) : [];
  }
  function formBody(form) {
    var data = Object.fromEntries(new FormData(form));
    Object.keys(data).forEach(function (key) {
      if (data[key] === "") delete data[key];
    });
    ["wall_time_minutes", "minutes"].forEach(function (key) {
      if (data[key]) data[key] = Number(data[key]);
    });
    return data;
  }
  function route(parts) {
    location.hash = "#/" + parts.join("/");
  }
  function destroyTimeline() {
    if (timelineController) {
      timelineController.destroy();
      timelineController = null;
    }
  }

  async function loadAgents() {
    try {
      var agents = await lines("/api/agents");
      agentCache = new Map(
        agents.map(function (agent) {
          return [agent.id, agent];
        }),
      );
      $("#connection").textContent = "connected";
      $("#connection").classList.remove("offline");
      $("#agent-count").textContent =
        agents.length +
        (agents.length === 1 ? " stored agent" : " stored agents");
      $("#agents").innerHTML = agents.length
        ? agents.map(agentCardHtml).join("")
        : '<div class="empty-state">No agents yet. Use the + button to start one.</div>';
      document.querySelectorAll(".agent-card").forEach(function (card) {
        card.onclick = function () {
          route(["agents", encodeURIComponent(card.dataset.id), "main"]);
        };
        card.onkeydown = function (event) {
          if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            card.click();
          }
        };
      });
      var inboxAgent = $("#inbox-agent"),
        selectedAgent = inboxAgent.value;
      inboxAgent.innerHTML =
        '<option value="">All agents</option>' +
        agents
          .map(function (agent) {
            return (
              '<option value="' +
              UI.escapeHtml(agent.id) +
              '">' +
              UI.escapeHtml(agent.name) +
              "</option>"
            );
          })
          .join("");
      inboxAgent.value = selectedAgent;
      return agents;
    } catch (error) {
      $("#connection").textContent = "disconnected";
      $("#connection").classList.add("offline");
      notice(error.message);
      return [];
    }
  }

  async function loadInbox() {
    if ($("#dashboard-page").classList.contains("hidden")) return;
    var form = $("#inbox-filters"),
      values = new FormData(form),
      limit = Number(values.get("limit") || 20),
      params = new URLSearchParams({
        limit: String(limit),
        offset: String(inboxOffset),
        priority: String(values.get("priority") || 2),
      }),
      agent = values.get("agent");
    if (agent) params.set("agent", agent);
    if (values.get("all")) params.set("all", "true");
    try {
      var notifications = await lines("/api/inbox?" + params.toString());
      $("#inbox").innerHTML = notifications.length
        ? notifications.map(notificationHtml).join("")
        : '<div class="empty-state">No matching notifications.</div>';
      $("#inbox-previous").disabled = inboxOffset === 0;
      $("#inbox-next").disabled = notifications.length < limit;
      $("#inbox-page").textContent =
        "Page " + (Math.floor(inboxOffset / limit) + 1);
    } catch (error) {
      notice(error.message);
    }
  }

  function notificationHtml(notification) {
    var summary = notification.summary || "",
      preview = summary.length > 240 ? summary.slice(0, 240) + "…" : summary,
      body =
        summary.length > 240
          ? '<details class="notification-summary"><summary>' +
            UI.escapeHtml(preview) +
            "</summary><div>" +
            UI.escapeHtml(summary) +
            "</div></details>"
          : '<div class="notification-summary">' +
            UI.escapeHtml(summary) +
            "</div>";
    return (
      '<article class="notification priority-' +
      notification.priority +
      '"><div class="notification-top"><span class="notification-priority">P' +
      notification.priority +
      '</span><button class="notification-agent text-button" type="button" data-agent="' +
      UI.escapeHtml(notification.agent_id) +
      '">' +
      UI.escapeHtml(notification.agent_name) +
      "</button>" +
      (notification.side_id
        ? '<span class="meta">Side ' + UI.escapeHtml(notification.side_id) + "</span>"
        : "") +
      '<span class="status ' +
      UI.escapeHtml(notification.status) +
      '">' +
      UI.escapeHtml(notification.status) +
      '</span><span class="meta notification-time">' +
      UI.escapeHtml(UI.humanTime(notification.timestamp)) +
      '</span></div><div class="event-kind">' +
      UI.escapeHtml(notification.event_type) +
      "</div>" +
      body +
      (notification.acknowledged
        ? '<span class="meta">Acknowledged</span>'
        : '<button class="ack-notification text-button" type="button" data-notification="' +
          UI.escapeHtml(notification.id) +
          '">Acknowledge through here</button>') +
      "</article>"
    );
  }
  function agentCardHtml(agent) {
    return (
      '<article class="agent-card" tabindex="0" data-id="' +
      UI.escapeHtml(agent.id) +
      '"><div><span class="agent-name">' +
      UI.escapeHtml(agent.name) +
      '</span><span class="status ' +
      UI.escapeHtml(agent.status) +
      '">' +
      UI.escapeHtml(agent.status) +
      '</span></div><div class="agent-path"><div class="meta">' +
      UI.escapeHtml(agent.dir) +
      '</div><div class="meta">' +
      UI.escapeHtml(agent.mode) +
      " · " +
      UI.escapeHtml(agent.ref) +
      " · run " +
      agent.run_number +
      " · " +
      UI.escapeHtml(agent.model) +
      " · " +
      agent.working_sides +
      " working side" +
      (agent.working_sides === 1 ? "" : "s") +
      '</div></div><div class="meta">' +
      UI.escapeHtml(agent.current_phase || agent.status) +
      '<br>progress ' +
      UI.escapeHtml(UI.humanTime(agent.last_event_at || agent.updated_at)) +
      "</div></article>"
    );
  }

  async function renderRoute() {
    destroyTimeline();
    var parts = location.hash
      .replace(/^#\/?/, "")
      .split("/")
      .filter(Boolean)
      .map(decodeURIComponent);
    if (parts[0] === "sides" && parts[1]) {
      await showSidePage(parts[1], parts[2] || "main");
      return;
    }
    if (parts[0] !== "agents" || !parts[1]) {
      selected = null;
      selectedSide = null;
      $("#agent-page").classList.add("hidden");
      $("#side-page").classList.add("hidden");
      $("#dashboard-page").classList.remove("hidden");
      document.title = "Subagent";
      await loadAgents();
      await loadInbox();
      return;
    }
    selected = parts[1];
    selectedSide = null;
    var tab = parts[2] || "main";
    await showAgent(selected, tab);
  }
  async function showAgent(id, tab) {
    if (!agentCache.has(id)) await loadAgents();
    try {
      var metadata = await api("/api/agents/" + encodeURIComponent(id)),
        item = agentCache.get(id),
        name = item ? item.name : id;
      if (selected !== id) return;
      selectedRef = metadata.ref;
      $("#dashboard-page").classList.add("hidden");
      $("#side-page").classList.add("hidden");
      $("#agent-page").classList.remove("hidden");
      document.title = name + " · Subagent";
      $("#agent-name").textContent = name;
      $("#crumb-name").textContent = name;
      $("#agent-status").textContent = metadata.status;
      $("#agent-status").className = "status " + metadata.status;
      $("#copy-id").textContent = metadata.ref + " · " + id;
      $("#rename-form [name=name]").value = name;
      $("#agent-meta").innerHTML =
        "<span>" +
        UI.escapeHtml(metadata.mode) +
        " mode</span><span>run " +
        metadata.run_number +
        "</span><span>" +
        UI.escapeHtml(metadata.model) +
        "</span><span>" +
        UI.escapeHtml(metadata.current_phase || metadata.status) +
        "</span><span>" +
        UI.escapeHtml(metadata.dir) +
        "</span><span>updated " +
        UI.escapeHtml(UI.humanTime(metadata.updated_at)) +
        "</span><span>model event " +
        UI.escapeHtml(UI.humanTime(metadata.last_model_event_at)) +
        "</span><span>tool event " +
        UI.escapeHtml(UI.humanTime(metadata.last_tool_event_at)) +
        "</span>" +
        (metadata.provider_request_id
          ? "<span>request " + UI.escapeHtml(metadata.provider_request_id) + "</span>"
          : "") +
        (metadata.retry_count
          ? "<span>retry " + metadata.retry_count + "</span>"
          : "");
      $("#stop").disabled = metadata.status !== "working";
      $("#time-form input").disabled = metadata.status !== "working";
      $("#time-form button").disabled = metadata.status !== "working";
      $("#delete").disabled = metadata.status === "working";
      var working = item ? item.working_sides : 0;
      $("#side-working-badge").textContent = working;
      $("#side-working-badge").classList.toggle("hidden", working === 0);
      showTab(tab);
    } catch (error) {
      notice(error.message);
      route([]);
    }
  }
  function showTab(tab) {
    if (!["main", "side", "controls"].includes(tab)) tab = "main";
    window.scrollTo(0, 0);
    document.querySelectorAll(".tab-page").forEach(function (page) {
      page.classList.add("hidden");
    });
    document.querySelectorAll("#agent-tabs button").forEach(function (button) {
      button.classList.toggle("active", button.dataset.tab === tab);
    });
    $("#" + tab + "-tab").classList.remove("hidden");
    if (tab === "main") openMain();
    if (tab === "side") openSide();
  }

  function TimelineController(options) {
    this.scroll = $(options.scroll);
    this.container = $(options.container);
    this.state = $(options.state);
    this.newButton = $(options.newButton);
    this.eventsUrl = options.eventsUrl;
    this.streamUrl = options.streamUrl;
    this.fullUrl = options.fullUrl;
    this.onTerminal = options.onTerminal || function () {};
    this.oldest = null;
    this.newest = null;
    this.loading = false;
    this.noMore = false;
    this.unseen = 0;
    this.stream = null;
    this.reconnectTimer = null;
    this.reconnectAttempt = 0;
    this.destroyed = false;
    this.terminal = false;
    var self = this;
    this.onScroll = function () {
      if (self.scroll.scrollTop < 80) self.loadOlder();
      if (self.nearBottom()) {
        self.unseen = 0;
        self.updateNewButton();
      }
    };
    this.scroll.addEventListener("scroll", this.onScroll);
    if (this.newButton)
      this.newButton.onclick = function () {
        self.scrollBottom();
        self.unseen = 0;
        self.updateNewButton();
      };
  }
  TimelineController.prototype.nearBottom = function () {
    return UI.isNearBottom(
      this.scroll.scrollHeight,
      this.scroll.scrollTop,
      this.scroll.clientHeight,
      120,
    );
  };
  TimelineController.prototype.scrollBottom = function () {
    this.scroll.scrollTop = this.scroll.scrollHeight;
  };
  TimelineController.prototype.updateNewButton = function () {
    if (!this.newButton) return;
    this.newButton.textContent = this.unseen
      ? this.unseen + " new event" + (this.unseen === 1 ? "" : "s")
      : "New events";
    this.newButton.classList.toggle("hidden", this.unseen === 0);
  };
  TimelineController.prototype.start = async function () {
    this.state.textContent = "Loading recent activity…";
    var events = await lines(
      this.eventsUrl + "?limit=50&types=" + encodeURIComponent(visibleTypes),
    );
    if (events.length) {
      this.oldest = events[0].event_id;
      this.newest = events[events.length - 1].event_id;
      this.container.innerHTML = events.map(eventHtml).join("");
      bindEventControls(this.container, this.fullUrl);
      var latestLifecycle = events
        .slice()
        .reverse()
        .find(function (event) {
          return event.type === "lifecycle" && event.data;
        });
      this.terminal = Boolean(
        latestLifecycle && latestLifecycle.data.status !== "working",
      );
    } else this.container.innerHTML = "";
    this.state.textContent = events.length
      ? "Scroll upward for older history"
      : "No activity yet";
    var self = this;
    requestAnimationFrame(function () {
      self.scrollBottom();
      if (!self.terminal) self.startStream();
    });
  };
  TimelineController.prototype.loadOlder = async function () {
    if (this.loading || this.noMore || !this.oldest) return;
    this.loading = true;
    this.state.textContent = "Loading older history…";
    var before = this.oldest,
      oldHeight = this.scroll.scrollHeight;
    try {
      var events = await lines(
        this.eventsUrl +
          "?limit=50&types=" +
          encodeURIComponent(visibleTypes) +
          "&before=" +
          encodeURIComponent(before),
      );
      if (!events.length) {
        this.noMore = true;
        this.state.textContent = "Beginning of history";
      } else {
        this.oldest = events[0].event_id;
        this.container.insertAdjacentHTML(
          "afterbegin",
          events.map(eventHtml).join(""),
        );
        bindEventControls(this.container, this.fullUrl);
        this.scroll.scrollTop = UI.anchoredScrollTop(
          this.scroll.scrollTop,
          oldHeight,
          this.scroll.scrollHeight,
        );
        this.state.textContent = "Scroll upward for older history";
      }
    } catch (error) {
      notice(error.message);
      this.state.textContent = "Could not load older history";
    }
    this.loading = false;
  };
  TimelineController.prototype.startStream = function () {
    if (this.destroyed || this.terminal) return;
    var self = this,
      url = this.streamUrl + "?types=" + encodeURIComponent(visibleTypes);
    if (this.newest) url += "&after=" + encodeURIComponent(this.newest);
    this.stream = new AuthEventStream(url, function (message) {
      var event = JSON.parse(message.data),
        follow = self.nearBottom();
      self.reconnectAttempt = 0;
      self.newest = event.event_id;
      if (!self.oldest) self.oldest = event.event_id;
      self.container.insertAdjacentHTML("beforeend", eventHtml(event));
      bindEventControls(self.container, self.fullUrl);
      if (follow)
        requestAnimationFrame(function () {
          self.scrollBottom();
        });
      else {
        self.unseen += 1;
        self.updateNewButton();
      }
      if (
        event.type === "lifecycle" &&
        event.data &&
        event.data.status !== "working"
      ) {
        self.terminal = true;
        self.onTerminal();
      }
    }, function (error) {
      self.stream = null;
      if (self.destroyed || self.terminal) return;
      if (error && self.reconnectAttempt === 0)
        notice("Live timeline disconnected; reconnecting…");
      var delay = UI.reconnectDelay(self.reconnectAttempt++);
      self.reconnectTimer = setTimeout(function () {
        self.reconnectTimer = null;
        self.startStream();
      }, delay);
    });
  };
  TimelineController.prototype.destroy = function () {
    this.destroyed = true;
    this.scroll.removeEventListener("scroll", this.onScroll);
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    if (this.stream) this.stream.close();
  };

  function eventSummary(event) {
    var data = event.data || {};
    if (event.type === "tool_call") {
      var args = UI.parseArguments(data) || {};
      if (data.name === "apply_patch") return "workspace patch";
      return String(
        args.path || args.command || args.pattern || args.query || "",
      )
        .split("\n")[0]
        .slice(0, 100);
    }
    if (event.type === "tool_result") {
      var summary = data.summary || {};
      return [
        summary.status,
        summary.exit_code != null ? "exit " + summary.exit_code : null,
        summary.path,
      ]
        .filter(Boolean)
        .join(" · ");
    }
    return "";
  }
  function toolPresentation(event) {
    var data = event.data || {},
      args = UI.parseArguments(data) || {},
      name = data.name || "tool",
      verb = name,
      target = args.path || args.workdir || "",
      additions = 0,
      deletions = 0;
    if (name === "apply_patch" && typeof args.patch === "string") {
      args.patch.split("\n").forEach(function (line) {
        if (/^\+[^+]/.test(line)) additions += 1;
        if (/^-[^-]/.test(line)) deletions += 1;
        var match = line.match(/^\*\*\* (?:Add|Update|Delete) File: (.+)$/);
        if (match && !target) target = match[1];
      });
      verb = "Edit";
    } else if (name === "edit") {
      verb = "Edit";
      additions = String(args.new_text || "").split("\n").length;
      deletions = String(args.old_text || "").split("\n").length;
    } else if (name === "write") verb = "Write";
    else if (name === "read") verb = "Explored";
    else if (name === "glob" || name === "grep") verb = "Explored";
    else if (name === "exec_command") {
      verb = "Shell";
      target = String(args.command || "").split("\n")[0].slice(0, 110);
    } else if (name === "view_image") verb = "Viewed";
    if (event.type === "tool_result") {
      verb = "Result";
      target = eventSummary(event);
    }
    return { verb: verb, target: target, additions: additions, deletions: deletions };
  }
  function eventHtml(event) {
    var data = event.data || {},
      isTool = event.type === "tool_call" || event.type === "tool_result",
      body = UI.eventBodyHtml(event);
    if (isTool) {
      var presentation = toolPresentation(event);
      body =
        '<div class="tool-card"><details class="tool-accordion" data-event="' +
        UI.escapeHtml(event.event_id) +
        '" data-kind="' +
        UI.escapeHtml(event.type) +
        '"><summary><div class="tool-summary"><strong>' +
        UI.escapeHtml(presentation.verb) +
        '</strong><span class="tool-target">' +
        UI.escapeHtml(presentation.target) +
        '</span><span class="tool-delta additions">' +
        (presentation.additions ? "+" + presentation.additions : "") +
        '</span><span class="tool-delta deletions">' +
        (presentation.deletions ? "-" + presentation.deletions : "") +
        '</span></div></summary><div class="tool-content">' +
        body +
        "</div></details></div>";
    }
    return (
      '<article class="event event-' +
      UI.escapeHtml(event.type) +
      '" data-event="' +
      UI.escapeHtml(event.event_id) +
      '"><div class="event-rail"><span class="event-kind">' +
      UI.escapeHtml(
        isTool
          ? (data.name || "tool") +
              " " +
              (event.type === "tool_call" ? "call" : "result")
          : event.type.replaceAll("_", " "),
      ) +
      '</span><time class="event-time">' +
      UI.escapeHtml(UI.humanTime(event.timestamp)) +
      '</time></div><div class="event-body">' +
      body +
      "</div></article>"
    );
  }
  function bindEventControls(root, fullUrl) {
    root
      .querySelectorAll(".tool-accordion:not([data-bound])")
      .forEach(function (details) {
        details.dataset.bound = "1";
        details.addEventListener("toggle", async function () {
          if (!details.open || details.dataset.loaded) return;
          try {
            var event = await api(
              fullUrl + "/" + encodeURIComponent(details.dataset.event),
            );
            details.querySelector(".tool-content").innerHTML =
              details.dataset.kind === "tool_result"
                ? UI.toolResultHtml(event.data || {})
                : UI.toolCallHtml(event.data || {});
            details.dataset.loaded = "1";
          } catch (error) {
            notice(error.message);
          }
        });
      });
    root
      .querySelectorAll(".load-full-patch:not([data-bound])")
      .forEach(function (button) {
        button.dataset.bound = "1";
        button.onclick = async function () {
          var article = button.closest(".event");
          try {
            var event = await api(
                fullUrl + "/" + encodeURIComponent(article.dataset.event),
              ),
              args = UI.parseArguments(event.data || {});
            if (args && args.patch) {
              article.querySelector(".diff").outerHTML = UI.patchDiffHtml(
                args.patch,
              );
              button.remove();
            }
          } catch (error) {
            notice(error.message);
          }
        };
      });
  }
  function AuthEventStream(url, onMessage, onClose) {
    var controller = new AbortController(),
      closed = false;
    function finish(error) {
      if (closed) return;
      closed = true;
      if (onClose) onClose(error || null);
    }
    fetch(url, {
      signal: controller.signal,
    })
      .then(async function (response) {
        if (!response.ok) throw new Error("Live timeline disconnected");
        var reader = response.body.getReader(),
          decoder = new TextDecoder(),
          buffer = "";
        while (true) {
          var part = await reader.read();
          if (part.done) break;
          buffer += decoder.decode(part.value, { stream: true });
          var blocks = buffer.split("\n\n");
          buffer = blocks.pop();
          blocks.forEach(function (block) {
            var data = block
              .split("\n")
              .filter(function (line) {
                return line.indexOf("data:") === 0;
              })
              .map(function (line) {
                return line.slice(5).trim();
              })
              .join("\n");
            if (data) onMessage({ data: data });
          });
        }
        finish(null);
      })
      .catch(function (error) {
        if (error.name !== "AbortError") finish(error);
      });
    this.close = function () {
      closed = true;
      controller.abort();
    };
  }

  async function openMain() {
    timelineController = new TimelineController({
      scroll: "#main-scroll",
      container: "#timeline",
      state: "#main-history-state",
      newButton: "#main-new-events",
      eventsUrl: "/api/agents/" + encodeURIComponent(selected) + "/events",
      streamUrl: "/api/agents/" + encodeURIComponent(selected) + "/stream",
      fullUrl: "/api/agents/" + encodeURIComponent(selected) + "/events",
    });
    await Promise.all([timelineController.start(), loadPendingMessages()]);
  }
  async function loadPendingMessages() {
    var messages = await lines(
        "/api/agents/" + encodeURIComponent(selected) + "/messages",
      ),
      pending = messages.filter(function (message) {
        return message.status === "pending";
      }),
      strip = $("#pending-messages");
    strip.classList.toggle("hidden", pending.length === 0);
    strip.innerHTML = pending
      .map(function (message) {
        return (
          '<div class="pending-item"><span>Queued: ' +
          UI.escapeHtml(message.content) +
          '</span><button data-cancel="' +
          UI.escapeHtml(message.id) +
          '" type="button">Cancel</button></div>'
        );
      })
      .join("");
    strip.querySelectorAll("[data-cancel]").forEach(function (button) {
      button.onclick = async function () {
        await api(
          "/api/agents/" +
            encodeURIComponent(selected) +
            "/messages/" +
            encodeURIComponent(button.dataset.cancel) +
            "/cancel",
          { method: "POST" },
        );
        loadPendingMessages();
      };
    });
  }

  async function openSide() {
    var sides = await lines(
      "/api/agents/" + encodeURIComponent(selected) + "/sides?limit=100",
    );
    renderSideList(sides);
  }
  function renderSideList(sides) {
    $("#side-list").innerHTML = sides.length
      ? sides
          .map(function (side) {
            return (
              '<article class="side-list-item" data-side="' +
              UI.escapeHtml(side.id) +
              '"><div class="side-question-preview"><span class="meta">' +
              UI.escapeHtml(side.ref) +
              "</span> " +
              UI.escapeHtml(side.question_preview) +
              '</div><div><span class="status ' +
              UI.escapeHtml(side.status) +
              '">' +
              UI.escapeHtml(side.status) +
              '</span></div><div class="side-list-meta"><span>' +
              side.tool_calls +
              " tools</span><span>" +
              UI.escapeHtml(UI.humanTime(side.created_at)) +
              "</span></div></article>"
            );
          })
          .join("")
      : '<div class="empty-state">No Side runs.</div>';
    document.querySelectorAll(".side-list-item").forEach(function (item) {
      item.onclick = function () {
        route(["sides", encodeURIComponent(item.dataset.side), "main"]);
      };
    });
  }
  async function showSidePage(sideId, tab) {
    var side = await api("/api/sides/" + encodeURIComponent(sideId));
    selected = side.agent_id;
    selectedSide = sideId;
    selectedSideRef = side.ref;
    if (!agentCache.has(selected)) await loadAgents();
    var parent = agentCache.get(selected);
    $("#dashboard-page").classList.add("hidden");
    $("#agent-page").classList.add("hidden");
    $("#side-page").classList.remove("hidden");
    document.title = "Side · " + (parent ? parent.name : selected);
    $("#side-page-title").textContent =
      "Side · " + (parent ? parent.name : "Agent");
    $("#side-page-status").textContent = side.status;
    $("#side-page-status").className = "status " + side.status;
    $("#copy-side-id").textContent = side.ref + " · " + side.id;
    $("#side-back-parent").textContent = parent ? parent.name : "Parent";
    $("#side-page-meta").innerHTML =
      "<span>readonly mode</span><span>" +
      side.tool_calls +
      " tool calls</span><span>" +
      UI.escapeHtml(side.model) +
      "</span><span>" +
      UI.escapeHtml(side.current_phase || side.status) +
      "</span><span>created " +
      UI.escapeHtml(UI.humanTime(side.created_at)) +
      "</span><span>progress " +
      UI.escapeHtml(UI.humanTime(side.last_event_at || side.updated_at)) +
      "</span>" +
      (side.provider_request_id
        ? "<span>request " + UI.escapeHtml(side.provider_request_id) + "</span>"
        : "");
    showSideTab(tab, side);
  }
  function showSideTab(tab, side) {
    if (!["main", "controls"].includes(tab)) tab = "main";
    window.scrollTo(0, 0);
    document.querySelectorAll(".side-tab-page").forEach(function (page) {
      page.classList.add("hidden");
    });
    document.querySelectorAll("#side-tabs button").forEach(function (button) {
      button.classList.toggle("active", button.dataset.tab === tab);
    });
    $("#side-" + tab + "-tab").classList.remove("hidden");
    $("#stop-side").disabled = side.status !== "working";
    $("#delete-side").disabled = side.status === "working";
    if (tab === "main") {
      timelineController = new TimelineController({
        scroll: "#side-scroll",
        container: "#side-timeline",
        state: "#side-history-state",
        newButton: "#side-new-events",
        eventsUrl: "/api/sides/" + encodeURIComponent(side.id) + "/events",
        streamUrl: "/api/sides/" + encodeURIComponent(side.id) + "/stream",
        fullUrl: "/api/sides/" + encodeURIComponent(side.id) + "/events",
        onTerminal: function () {
          destroyTimeline();
          showSidePage(side.id, "main");
        },
      });
      timelineController.start();
    }
  }

  $("#spawn-form").onsubmit = async function (event) {
    event.preventDefault();
    try {
      var agent = await api("/api/agents", {
        method: "POST",
        body: JSON.stringify(formBody(event.target)),
      });
      event.target.reset();
      $("#spawn-dialog").close();
      await loadAgents();
      route(["agents", encodeURIComponent(agent.id), "main"]);
    } catch (error) {
      notice(error.message);
    }
  };
  $("#send-form").onsubmit = async function (event) {
    event.preventDefault();
    var input = event.target.elements.message,
      message = input.value.trim();
    try {
      var response = await api(
        "/api/agents/" + encodeURIComponent(selected) + "/send",
        { method: "POST", body: JSON.stringify({ message: message }) },
      );
      input.value = "";
      notice(response);
      await loadPendingMessages();
    } catch (error) {
      notice(error.message);
    }
  };
  $("#side-form").onsubmit = async function (event) {
    event.preventDefault();
    try {
      var response = await api(
        "/api/agents/" + encodeURIComponent(selected) + "/sides",
        { method: "POST", body: JSON.stringify(formBody(event.target)) },
      );
      event.target.reset();
      event.target.elements.wall_time_minutes.value = "2";
      $("#side-dialog").close();
      await loadAgents();
      route(["sides", encodeURIComponent(response.id), "main"]);
    } catch (error) {
      notice(error.message);
    }
  };
  $("#rename-form").onsubmit = async function (event) {
    event.preventDefault();
    try {
      var response = await api(
        "/api/agents/" + encodeURIComponent(selected) + "/rename",
        { method: "POST", body: JSON.stringify(formBody(event.target)) },
      );
      notice(response);
      await loadAgents();
      await showAgent(selected, "controls");
    } catch (error) {
      notice(error.message);
    }
  };
  $("#time-form").onsubmit = async function (event) {
    event.preventDefault();
    try {
      await api("/api/agents/" + encodeURIComponent(selected) + "/time", {
        method: "POST",
        body: JSON.stringify(formBody(event.target)),
      });
      notice("Deadline updated");
      await showAgent(selected, "controls");
    } catch (error) {
      notice(error.message);
    }
  };
  $("#stop").onclick = async function () {
    try {
      await api("/api/agents/" + encodeURIComponent(selected) + "/stop", {
        method: "POST",
      });
      notice("Agent stopped");
      await loadAgents();
      await showAgent(selected, "controls");
    } catch (error) {
      notice(error.message);
    }
  };
  $("#delete").onclick = async function () {
    if (
      !confirm(
        "Delete this Agent and all Side history? Workspace files remain untouched.",
      )
    )
      return;
    try {
      await api("/api/agents/" + encodeURIComponent(selected), {
        method: "DELETE",
      });
      notice("Agent deleted");
      route([]);
    } catch (error) {
      notice(error.message);
    }
  };

  $("#agent-tabs")
    .querySelectorAll("button")
    .forEach(function (button) {
      button.onclick = function () {
        route(["agents", encodeURIComponent(selected), button.dataset.tab]);
      };
    });
  $("#side-tabs")
    .querySelectorAll("button")
    .forEach(function (button) {
      button.onclick = function () {
        route(["sides", encodeURIComponent(selectedSide), button.dataset.tab]);
      };
    });
  $("#home-link").onclick = function () {
    route([]);
  };
  $("#back-dashboard").onclick = function () {
    route([]);
  };
  $("#side-back-agents").onclick = function () {
    route([]);
  };
  $("#side-back-parent").onclick = function () {
    route(["agents", encodeURIComponent(selected), "side"]);
  };
  $("#copy-id").onclick = function () {
    navigator.clipboard.writeText(selectedRef || selected);
    notice("Agent reference copied");
  };
  $("#copy-side-id").onclick = function () {
    navigator.clipboard.writeText(selectedSideRef || selectedSide);
    notice("Side reference copied");
  };
  $("#stop-side").onclick = async function () {
    try {
      await api("/api/sides/" + encodeURIComponent(selectedSide) + "/stop", {
        method: "POST",
      });
      notice("Side stopped");
      destroyTimeline();
      await showSidePage(selectedSide, "controls");
    } catch (error) {
      notice(error.message);
    }
  };
  $("#delete-side").onclick = async function () {
    if (!confirm("Delete this Side history and its saved tool outputs?"))
      return;
    try {
      await api("/api/sides/" + encodeURIComponent(selectedSide), {
        method: "DELETE",
      });
      notice("Side history deleted");
      route(["agents", encodeURIComponent(selected), "side"]);
    } catch (error) {
      notice(error.message);
    }
  };
  $("#refresh").onclick = loadAgents;
  $("#refresh-inbox").onclick = loadInbox;
  $("#inbox-filters").onchange = function () {
    inboxOffset = 0;
    loadInbox();
  };
  $("#inbox-previous").onclick = function () {
    var limit = Number($("#inbox-filters [name=limit]").value);
    inboxOffset = Math.max(0, inboxOffset - limit);
    loadInbox();
  };
  $("#inbox-next").onclick = function () {
    inboxOffset += Number($("#inbox-filters [name=limit]").value);
    loadInbox();
  };
  $("#inbox").onclick = function (event) {
    var acknowledge = event.target.closest(".ack-notification");
    if (acknowledge) {
      api("/api/inbox/ack/" + encodeURIComponent(acknowledge.dataset.notification), {
        method: "POST",
      }).then(function () {
        inboxOffset = 0;
        loadInbox();
      }).catch(function (error) { notice(error.message); });
      return;
    }
    var button = event.target.closest(".notification-agent");
    if (button) route(["agents", encodeURIComponent(button.dataset.agent), "main"]);
  };
  $("#open-spawn").onclick = function () {
    $("#spawn-dialog").showModal();
  };
  $("#close-spawn").onclick = $("#cancel-spawn").onclick = function () {
    $("#spawn-dialog").close();
  };
  $("#open-side").onclick = function () {
    $("#side-dialog").showModal();
  };
  $("#new-side-from-page").onclick = function () {
    $("#side-dialog").showModal();
  };
  $("#close-side").onclick = $("#cancel-side").onclick = function () {
    $("#side-dialog").close();
  };
  window.addEventListener("hashchange", renderRoute);
  setInterval(loadInbox, 5000);
  renderRoute();
})();
