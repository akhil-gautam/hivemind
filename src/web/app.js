/* Hive web UI — plain JS, no build step, no dependencies. */
(function () {
  "use strict";

  var $ = function (id) { return document.getElementById(id); };

  var mesh = { self: null, nodes: [] };
  var selectedId = null;
  var stream = null;
  var active = null;      // { node: nodeObj, id: taskId }
  var agentsLoaded = false;

  function getJSON(url, opts) {
    return fetch(url, opts).then(function (r) {
      return r.text().then(function (text) {
        var body = null;
        try { body = text ? JSON.parse(text) : null; } catch (e) { body = null; }
        if (!r.ok) {
          throw new Error((body && body.error) || r.statusText || ("HTTP " + r.status));
        }
        return body;
      });
    });
  }

  function postJSON(url, data) {
    return getJSON(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data || {})
    });
  }

  function stateClass(s) {
    if (s === "done") return "done";
    if (s === "failed" || s === "cancelled") return "failed";
    if (s === "running") return "running";
    return "";
  }

  function dotClass(node) {
    if (!node.online) return "off";
    if (node.busy) return "busy";
    return "on";
  }

  function agentLabel(node) {
    if (!node.agent) return "no agent set up";
    return node.agent;
  }

  function selectNode(id) {
    selectedId = id;
    var node = nodeById(id);
    $("targetName").textContent = node ? node.name : "this Mac";
    renderMesh();
  }

  function nodeById(id) {
    for (var i = 0; i < mesh.nodes.length; i++) {
      if (mesh.nodes[i].id === id) return mesh.nodes[i];
    }
    return null;
  }

  /// Pull the current state of the mesh and redraw everything.
  function refresh() {
    return getJSON("/v1/mesh").then(function (data) {
      mesh = data;
      if (!selectedId || !nodeById(selectedId)) {
        selectedId = mesh.nodes.length ? mesh.nodes[0].id : null;
        var node = nodeById(selectedId);
        $("targetName").textContent = node ? node.name : "this Mac";
      }
      renderSelf();
      renderMesh();
      renderSetup();
      return renderRecent();
    }).catch(function (e) {
      $("meshHint").textContent = "Cannot reach this Mac: " + e.message;
    });
  }

  function renderMesh() {
    var box = $("mesh");
    box.textContent = "";

    if (!mesh.nodes.length) {
      $("meshHint").textContent = "No other Macs found yet. Make sure Hive is running on them.";
      return;
    }
    $("meshHint").textContent = "Pick the Mac that should do the work.";

    mesh.nodes.forEach(function (node) {
      var card = document.createElement("button");
      card.className = "mac";
      card.type = "button";
      card.setAttribute("aria-pressed", node.id === selectedId ? "true" : "false");

      var name = document.createElement("div");
      name.className = "name";
      var dot = document.createElement("span");
      dot.className = "dot " + dotClass(node);
      var label = document.createElement("span");
      label.textContent = node.name;
      name.appendChild(dot);
      name.appendChild(label);

      var agent = document.createElement("div");
      agent.className = "agent";
      agent.textContent = agentLabel(node);

      var where = document.createElement("div");
      where.className = "where";
      where.textContent = node.online
        ? (node.busy ? "busy now" : "ready")
        : "not reachable";

      card.appendChild(name);
      card.appendChild(agent);
      card.appendChild(where);
      card.addEventListener("click", function () { selectNode(node.id); });
      box.appendChild(card);
    });
  }

  function renderSelf() {
    if (!mesh.self) return;
    var s = mesh.self;
    $("selfLine").textContent = s.name + " · " + s.version +
      (s.work_dir ? " · " + s.work_dir : "");
    if (!s.work_dir) {
      $("taskDir").placeholder = "e.g. ~/projects/my-app";
    }
  }

  function renderSetup() {
    var s = mesh.self;
    if (!s) return;
    if (s.agent) {
      $("setupCard").hidden = true;
      return;
    }
    $("setupCard").hidden = false;
    if (agentsLoaded) return;
    agentsLoaded = true;
    getJSON("/v1/agents").then(function (data) {
      var box = $("agentChoices");
      box.textContent = "";
      if (!data.agents.length) {
        var p = document.createElement("p");
        p.className = "lede";
        p.textContent = "No agent program was found on this Mac. Install one " +
          "(Claude Code, OpenCode, Codex…) and refresh this page.";
        box.appendChild(p);
        return;
      }
      data.agents.forEach(function (agent) {
        var btn = document.createElement("button");
        btn.className = "choice";
        btn.type = "button";
        var title = document.createElement("span");
        title.textContent = agent.label;
        var cmd = document.createElement("span");
        cmd.className = "cmd";
        cmd.textContent = agent.cmd;
        btn.appendChild(title);
        btn.appendChild(cmd);
        btn.addEventListener("click", function () {
          $("workDir").value = data.work_dir || "";
          postJSON("/v1/settings", { agent_cmd: agent.cmd }).then(function () {
            return refresh();
          });
        });
        box.appendChild(btn);
      });
    }).catch(function () { /* the setup card stays empty */ });
  }

  function renderRecent() {
    var calls = mesh.nodes.filter(function (n) { return n.online; }).map(function (node) {
      return getJSON(node.base + "/v1/tasks").then(function (data) {
        return (data.tasks || []).map(function (t) {
          t.node_name = node.name;
          t.node_base = node.base;
          return t;
        });
      }).catch(function () { return []; });
    });

    return Promise.all(calls).then(function (lists) {
      var all = [];
      lists.forEach(function (l) { all = all.concat(l); });
      all.sort(function (a, b) { return (b.created_ms || 0) - (a.created_ms || 0); });
      all = all.slice(0, 8);

      var list = $("recent");
      list.textContent = "";
      if (!all.length) {
        $("recentHint").textContent = "Nothing yet.";
        return;
      }
      $("recentHint").textContent = "";
      all.forEach(function (task) {
        var li = document.createElement("li");
        var what = document.createElement("span");
        what.className = "what";
        what.textContent = task.prompt || "(no prompt)";
        var where = document.createElement("span");
        where.className = "where";
        where.textContent = task.node_name;
        var state = document.createElement("span");
        state.className = "state " + stateClass(task.state);
        state.textContent = task.state;
        li.appendChild(what);
        li.appendChild(where);
        li.appendChild(state);
        li.style.cursor = "pointer";
        li.addEventListener("click", function () { showTask(nodeByTask(task), task.id); });
        list.appendChild(li);
      });
    });
  }

  function nodeByTask(task) {
    for (var i = 0; i < mesh.nodes.length; i++) {
      if (mesh.nodes[i].name === task.node_name) return mesh.nodes[i];
    }
    return mesh.self;
  }

  function showTask(node, id) {
    if (!node) return;
    $("runCard").hidden = false;
    $("output").textContent = "";
    active = { node: node, id: id };
    getJSON(node.base + "/v1/tasks/" + id).then(function (task) {
      $("output").textContent = task.output || task.error || "";
      setState(task.state);
    }).catch(function (e) {
      $("output").textContent = e.message;
    });
  }

  function setState(s) {
    var el = $("runState");
    el.textContent = s || "";
    el.className = "state " + stateClass(s);
  }

  function append(text) {
    var out = $("output");
    out.textContent += text;
    out.scrollTop = out.scrollHeight;
  }

  function run() {
    var prompt = $("prompt").value.trim();
    if (!prompt) {
      showHint("Type what you want the agent to do first.");
      return;
    }
    var node = nodeById(selectedId) || mesh.self;
    if (!node) {
      showHint("No Mac is available to run this.");
      return;
    }
    if (!node.online) {
      showHint(node.name + " is not reachable right now.");
      return;
    }

    var body = { prompt: prompt };
    var dir = $("taskDir").value.trim();
    if (dir) body.cwd = dir;

    $("run").disabled = true;
    showHint("");
    $("runCard").hidden = false;
    $("output").textContent = "";
    setState("starting");

    postJSON(node.base + "/v1/tasks", body).then(function (task) {
      active = { node: node, id: task.id };
      if (stream) stream.close();
      var es = new EventSource(node.base + "/v1/tasks/" + task.id + "/events");
      stream = es;
      es.addEventListener("output", function (ev) {
        append(JSON.parse(ev.data).chunk);
      });
      es.addEventListener("stderr", function (ev) {
        append(JSON.parse(ev.data).chunk);
      });
      es.addEventListener("final", function (ev) {
        var v = JSON.parse(ev.data);
        setState(v.state);
        es.close();
        stream = null;
        $("run").disabled = false;
        if (v.error) showHint(v.error);
        refresh();
      });
      es.onerror = function () {
        $("run").disabled = false;
      };
    }).catch(function (e) {
      $("run").disabled = false;
      showHint(e.message);
    });
  }

  function showHint(text) {
    var el = $("runHint");
    el.textContent = text || "";
    el.className = text ? "hint bad" : "hint";
  }

  function cancel() {
    if (!active) return;
    var node = active.node;
    var id = active.id;
    postJSON(node.base + "/v1/tasks/" + id + "/cancel", {}).then(function () {
      showHint("Stopped.");
    }).catch(function (e) {
      showHint(e.message);
    });
  }

  function saveSetup() {
    var payload = {};
    var dir = $("workDir").value.trim();
    if (dir) payload.work_dir = dir;
    postJSON("/v1/settings", payload).then(function () {
      return refresh();
    }).catch(function (e) {
      showHint(e.message);
    });
  }

  function useCustom() {
    var cmd = $("customCmd").value.trim();
    if (!cmd) {
      showHint("Type the command to run first.");
      return;
    }
    postJSON("/v1/settings", { agent_cmd: cmd }).then(function () {
      return refresh();
    }).catch(function (e) {
      showHint(e.message);
    });
  }

  $("run").addEventListener("click", run);
  $("cancel").addEventListener("click", cancel);
  $("saveSetup").addEventListener("click", saveSetup);
  $("useCustom").addEventListener("click", useCustom);
  $("prompt").addEventListener("keydown", function (e) {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") run();
  });

  refresh();
  setInterval(refresh, 6000);
})();
