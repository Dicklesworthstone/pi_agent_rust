'use strict';

(() => {
  const element = id => document.getElementById(id);
  let token = new URLSearchParams(location.hash.slice(1)).get('token');
  history.replaceState(null, '', location.pathname);
  let credential = null;
  let sequence = 0;
  let generation = 0;
  let stopped = false;
  let viewOnly = true;
  let busy = true;
  let ready = false;
  let pending = false;
  let requesting = false;
  let transcript = '';
  let failures = 0;
  let timer = null;

  const bytes = text => new TextEncoder().encode(text).length;
  const notice = text => {
    element('notice').textContent = text;
    element('notice').hidden = !text;
  };
  const updateComposer = () => {
    const length = bytes(element('prompt').value);
    element('prompt-count').textContent = `${length.toLocaleString()} / 8 KiB`;
    element('prompt').disabled = stopped || viewOnly || !ready || pending;
    element('submit').disabled = stopped || viewOnly || !ready || busy || pending || requesting || length === 0 || length > 8192;
  };
  const stop = text => {
    stopped = true;
    token = null;
    credential = null;
    clearTimeout(timer);
    element('connection').textContent = 'Disconnected';
    element('signal').classList.remove('connected');
    element('disconnect').hidden = true;
    element('status').textContent = 'Offline';
    notice(text);
    updateComposer();
  };
  const request = async (path, body, authenticated = true) => {
    const headers = {'Content-Type': 'application/json'};
    if (authenticated && credential) headers.Authorization = `Bearer ${credential}`;
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 6000);
    try {
      return await fetch(path, {
        method: 'POST', headers, body: JSON.stringify(body), cache: 'no-store',
        credentials: 'omit', redirect: 'error', signal: controller.signal,
      });
    } finally {
      clearTimeout(timeout);
    }
  };
  const renderMessages = messages => {
    const next = JSON.stringify(messages);
    if (next === transcript) return;
    transcript = next;
    if (!messages.length) {
      const empty = document.createElement('div');
      empty.className = 'empty';
      const title = document.createElement('h3');
      title.textContent = 'Ready when you are.';
      const description = document.createElement('p');
      description.textContent = 'Completed conversation messages will appear here. Continue in the terminal or propose the next instruction.';
      empty.append(title, description);
      element('messages').replaceChildren(empty);
      return;
    }
    const fragment = document.createDocumentFragment();
    for (const message of messages) {
      const card = document.createElement('article');
      const role = ['user', 'assistant', 'tool', 'notice'].includes(message.role) ? message.role : 'notice';
      card.className = `message ${role}`;
      const label = document.createElement('h3');
      label.textContent = {user: 'You', assistant: 'Pi', tool: 'Tool activity', notice: 'Session notice'}[role];
      const content = document.createElement('div');
      content.className = 'message-text';
      content.textContent = message.text;
      card.append(label, content);
      fragment.append(card);
    }
    element('messages').replaceChildren(fragment);
  };
  const poll = async () => {
    if (stopped) return;
    try {
      const response = await request('/api/snapshot', {after: sequence});
      if (stopped) return;
      if (response.status === 401) {
        stop('Access expired or was revoked. Use /web in the local terminal for a new link.');
        return;
      }
      if (response.status !== 204) {
        if (!response.ok) throw new Error('Session update unavailable.');
        const data = await response.json();
        if (stopped) return;
        sequence = data.seq;
        generation = data.generation;
        busy = data.busy;
        ready = data.ready;
        viewOnly = data.view_only;
        pending = data.proposal?.status === 'pending';
        element('session-name').textContent = data.snapshot.name;
        element('model').textContent = data.snapshot.model || 'Local Pi session';
        element('status').textContent = !ready ? 'Starting' : busy ? 'Working locally' : 'Ready';
        element('status').classList.toggle('working', busy);
        element('omitted').hidden = !data.snapshot.omitted;
        renderMessages(data.snapshot.messages);
        element('proposal-status').textContent = viewOnly
          ? 'This is a view-only connection.'
          : data.proposal?.reason || 'Prompts run only after acceptance in the local terminal.';
        updateComposer();
      }
      failures = 0;
      notice('');
      element('connection').textContent = viewOnly ? 'Connected · view only' : 'Connected · supervised';
      element('signal').classList.add('connected');
    } catch (_) {
      if (stopped) return;
      failures += 1;
      element('connection').textContent = 'Reconnecting';
      element('signal').classList.remove('connected');
      notice('The local session is temporarily unavailable. Reconnecting…');
      if (failures >= 8) {
        stop('The Pi session stopped or is unreachable. Return to the local terminal to reconnect.');
        return;
      }
    }
    if (!stopped) timer = setTimeout(poll, failures ? 1500 : 700);
  };

  element('prompt').addEventListener('input', updateComposer);
  element('proposal-form').addEventListener('submit', async event => {
    event.preventDefault();
    if (element('submit').disabled || !credential) return;
    requesting = true;
    updateComposer();
    try {
      const response = await request('/api/proposal', {generation, text: element('prompt').value});
      const data = await response.json();
      if (!response.ok) throw new Error(data.error || 'The proposal was not accepted.');
      pending = true;
      sequence = 0;
      element('proposal-status').textContent = 'Waiting for explicit acceptance in the local terminal.';
    } catch (error) {
      element('proposal-status').textContent = error.message || 'Could not submit the proposal.';
    } finally {
      requesting = false;
      updateComposer();
    }
  });
  element('disconnect').addEventListener('click', async () => {
    try { await request('/api/leave', {}); } catch (_) { /* Local revocation or exit already closed access. */ }
    stop('Disconnected. Use /web in the local terminal for a fresh pairing link.');
  });
  window.addEventListener('pagehide', () => {
    if (credential) {
      fetch('/api/leave', {
        method: 'POST', headers: {'Content-Type': 'application/json', Authorization: `Bearer ${credential}`},
        body: '{}', credentials: 'omit', cache: 'no-store', keepalive: true,
      }).catch(() => {});
    }
    credential = null;
    stopped = true;
    clearTimeout(timer);
  });

  const connect = async () => {
    if (!token) {
      stop('Open a fresh pairing link from /web in the local Pi terminal. Links are single-use and expire after ten minutes.');
      return;
    }
    try {
      const response = await request('/api/pair', {token}, false);
      token = null;
      const data = await response.json();
      if (stopped) return;
      if (!response.ok) throw new Error(data.error || 'Pairing failed.');
      credential = data.credential;
      viewOnly = data.view_only;
      element('disconnect').hidden = false;
      await poll();
    } catch (error) {
      stop(error.message || 'Could not connect to the local Pi session.');
    }
  };
  connect();
})();
