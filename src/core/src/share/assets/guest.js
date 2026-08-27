(() => {
  "use strict";

  const MAX_FILES = 8;
  const MAX_FILE_BYTES = 15 * 1024 * 1024;
  const MAX_TOTAL_BYTES = 30 * 1024 * 1024;
  const MAX_REQUEST_BYTES = 32 * 1024 * 1024;
  const requestEncoder = new TextEncoder();
  const fragment = new URLSearchParams(location.hash.slice(1));
  const key = fragment.get("key") || "";
  history.replaceState(null, "", `${location.pathname}${location.search}`);

  const conversation = document.querySelector("#conversation");
  const empty = document.querySelector("#empty-state");
  const form = document.querySelector("#composer");
  const prompt = document.querySelector("#prompt");
  const send = document.querySelector("#send");
  const filesInput = document.querySelector("#files");
  const attachmentTray = document.querySelector("#attachments");
  const status = document.querySelector("#status");
  const statusDot = document.querySelector("#status-dot");
  let model = "";
  let busy = false;
  let attachments = [];
  const messages = [];

  const authHeaders = () => ({ Authorization: `Bearer ${key}` });
  const setStatus = (label, state) => {
    status.textContent = label;
    statusDot.className = `dot ${state || ""}`;
  };
  const addMessage = (role, text, className = "") => {
    empty?.remove();
    const node = document.createElement("article");
    node.className = `message ${role} ${className}`;
    const meta = document.createElement("span");
    meta.className = "meta";
    meta.textContent = role === "user" ? "You" : "Blackwall";
    const content = document.createElement("span");
    content.textContent = text;
    node.append(meta, content);
    conversation.append(node);
    node.scrollIntoView({ block: "end", behavior: "smooth" });
    return content;
  };
  const apiError = async (response) => {
    const payload = await response.json().catch(() => null);
    return payload?.error?.message || `Request failed (${response.status})`;
  };

  const connect = async () => {
    if (!key.startsWith("bw1_")) {
      setStatus("Invite key missing", "err");
      send.disabled = true;
      addMessage("assistant", "This invite link is incomplete. Ask the host for a new link.", "error");
      return;
    }
    try {
      const response = await fetch("/v1/models", { headers: authHeaders(), cache: "no-store" });
      if (!response.ok) throw new Error(await apiError(response));
      const payload = await response.json();
      model = payload.data?.[0]?.id || "";
      if (!model) throw new Error("The host did not assign a model.");
      setStatus(model, "ok");
    } catch (error) {
      setStatus("Disconnected", "err");
      send.disabled = true;
      addMessage("assistant", error instanceof Error ? error.message : "Could not connect.", "error");
    }
  };

  const renderAttachments = () => {
    attachmentTray.replaceChildren();
    attachments.forEach((attachment, index) => {
      const node = document.createElement("div");
      node.className = "attachment";
      if (attachment.preview) {
        const image = document.createElement("img");
        image.src = attachment.preview;
        image.alt = "";
        node.append(image);
      }
      const label = document.createElement("span");
      label.textContent = attachment.file.name;
      const remove = document.createElement("button");
      remove.type = "button";
      remove.setAttribute("aria-label", `Remove ${attachment.file.name}`);
      remove.textContent = "×";
      remove.addEventListener("click", () => {
        const [removed] = attachments.splice(index, 1);
        if (removed?.preview) URL.revokeObjectURL(removed.preview);
        renderAttachments();
      });
      node.append(label, remove);
      attachmentTray.append(node);
    });
  };

  const addFiles = (incoming) => {
    const next = [...incoming].slice(0, Math.max(0, MAX_FILES - attachments.length));
    const currentBytes = attachments.reduce((total, item) => total + item.file.size, 0);
    let addedBytes = 0;
    for (const file of next) {
      if (file.size > MAX_FILE_BYTES || currentBytes + addedBytes + file.size > MAX_TOTAL_BYTES) continue;
      addedBytes += file.size;
      attachments.push({ file, preview: file.type.startsWith("image/") ? URL.createObjectURL(file) : "" });
    }
    renderAttachments();
  };

  const readAttachment = (item) => new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error(`Could not read ${item.file.name}`));
    reader.onload = () => resolve({ file: item.file, value: String(reader.result || "") });
    if (item.file.type.startsWith("image/")) reader.readAsDataURL(item.file);
    else reader.readAsText(item.file);
  });

  const messageContent = async (text, selected) => {
    if (!selected.length) return text;
    const parts = [{ type: "text", text }];
    for (const item of selected) {
      const read = await readAttachment(item);
      if (read.file.type.startsWith("image/")) {
        parts.push({ type: "image_url", image_url: { url: read.value } });
      } else {
        parts[0].text += `\n\n<attachment name=${JSON.stringify(read.file.name)}>\n${read.value}\n</attachment>`;
      }
    }
    return parts;
  };

  const encodeRequest = (history, content) => requestEncoder.encode(JSON.stringify({
    model,
    messages: [...history, { role: "user", content }],
    stream: true
  }));

  const prepareRequest = (content) => {
    const currentOnly = encodeRequest([], content);
    if (currentOnly.byteLength > MAX_REQUEST_BYTES) {
      throw new Error(
        "This message is too large to send safely. Remove one or more attachments and try again (32 MB request limit)."
      );
    }

    let retained = messages;
    let droppedMessages = 0;
    let body = retained.length ? encodeRequest(retained, content) : currentOnly;
    while (body.byteLength > MAX_REQUEST_BYTES && retained.length) {
      const turnLength = retained[0]?.role === "user" && retained[1]?.role === "assistant" ? 2 : 1;
      droppedMessages += turnLength;
      retained = retained.slice(turnLength);
      body = encodeRequest(retained, content);
    }

    return { body, droppedMessages };
  };

  filesInput.addEventListener("change", () => { addFiles(filesInput.files || []); filesInput.value = ""; });
  prompt.addEventListener("input", () => {
    prompt.style.height = "auto";
    prompt.style.height = `${Math.min(prompt.scrollHeight, 150)}px`;
  });
  prompt.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      form.requestSubmit();
    }
  });
  document.addEventListener("paste", (event) => {
    const imageFiles = [...(event.clipboardData?.files || [])].filter((file) => file.type.startsWith("image/"));
    if (imageFiles.length) addFiles(imageFiles);
  });
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const submittedPrompt = prompt.value;
    const text = submittedPrompt.trim();
    if (busy || !model || (!text && !attachments.length)) return;
    busy = true;
    send.disabled = true;
    const selected = attachments.slice();
    let assistant = null;
    let attachmentsConsumed = false;
    try {
      const content = await messageContent(text, selected);
      const request = prepareRequest(content);

      attachments = attachments.filter((item) => !selected.includes(item));
      attachmentsConsumed = true;
      renderAttachments();
      if (prompt.value === submittedPrompt) {
        prompt.value = "";
        prompt.style.height = "auto";
      }
      addMessage("user", text || selected.map((item) => item.file.name).join(", "));
      if (request.droppedMessages) {
        addMessage("assistant", "Earlier messages were left out to keep this request within the 32 MB safety limit.");
      }
      assistant = addMessage("assistant", "");
      const response = await fetch("/v1/chat/completions", {
        method: "POST",
        headers: { ...authHeaders(), "Content-Type": "application/json" },
        body: request.body
      });
      if (!response.ok) throw new Error(await apiError(response));
      if (!response.body) throw new Error("The model returned no response stream.");
      messages.splice(0, request.droppedMessages);
      messages.push({ role: "user", content });
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      let full = "";
      while (true) {
        const { value, done } = await reader.read();
        buffer += decoder.decode(value || new Uint8Array(), { stream: !done });
        const frames = buffer.split(/\r?\n\r?\n/);
        buffer = done ? "" : frames.pop() || "";
        for (const frame of frames) {
          const data = frame.split(/\r?\n/).filter((line) => line.startsWith("data:"))
            .map((line) => line.slice(5).trimStart()).join("\n");
          if (!data || data === "[DONE]") continue;
          const chunk = JSON.parse(data);
          const delta = chunk.choices?.[0]?.delta?.content || "";
          full += delta;
          assistant.textContent = full;
          assistant.parentElement?.scrollIntoView({ block: "end" });
        }
        if (done) break;
      }
      messages.push({ role: "assistant", content: full });
    } catch (error) {
      const message = error instanceof Error ? error.message : "The request failed.";
      assistant = assistant || addMessage("assistant", message, "error");
      assistant.textContent = message;
      assistant.parentElement?.classList.add("error");
    } finally {
      if (attachmentsConsumed) {
        for (const item of selected) if (item.preview) URL.revokeObjectURL(item.preview);
      }
      busy = false;
      send.disabled = false;
      prompt.focus();
    }
  });

  connect();
})();
