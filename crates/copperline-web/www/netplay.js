// SPDX-License-Identifier: GPL-3.0-or-later

import { NetplayDiagnostics, connectionFailure } from './netplay-diagnostics.js';
import { RoomClient, inviteUrl, roomFromInvite } from './netplay-room.js';
import qrcode from './netplay-qr.js';
import { MEDIA_CHANNEL, MEDIA_VERSION, MediaTransfer } from './netplay-media.js';
import { SWAP_CHANNEL, SWAP_VERSION, DISK_LIMIT, DiskSwaps } from './netplay-swap.js';

// Signaling uses expiring room invitations or manual copy/paste codes.
// Only bounded input packets use the data channel.
const CODE_LIMIT = 96 * 1024;
export const PACKET_LIMIT = 1103;
const QUEUE_LIMIT = 64;
const CHANNEL = 'copperline-netplay-v1';

export function validateSettings(value) {
  if (!value || !/^[0-9a-f]{32}$/i.test(value.session ?? '') ||
      !Number.isInteger(value.delay) || value.delay < 0 || value.delay > 6 ||
      !Number.isInteger(value.window) || value.window < 1 || value.window > 12 ||
      !['joystick', 'cd32', 'mouse'].includes(value.controller) ||
      (value.media !== undefined && value.media !== MEDIA_VERSION) ||
      (value.swaps !== undefined && value.swaps !== SWAP_VERSION)) {
    throw new Error('Invalid netplay settings in connection code');
  }
  return { session: value.session.toLowerCase(), delay: value.delay,
    window: value.window, controller: value.controller,
    ...(value.media ? { media: value.media } : {}), ...(value.swaps ? { swaps: value.swaps } : {}) };
}

export function encodeCode(description, settings) {
  const code = 'CLNP1.' + btoa(JSON.stringify({ description, settings: validateSettings(settings) }));
  if (code.length > CODE_LIMIT) throw new Error('Connection code is too large');
  return code;
}

export function decodeCode(code, type) {
  code = code.trim();
  if (code.length > CODE_LIMIT || !code.startsWith('CLNP1.')) {
    throw new Error('Paste a Copperline connection code');
  }
  let value;
  try { value = JSON.parse(atob(code.slice(6))); }
  catch { throw new Error('Connection code is incomplete or damaged'); }
  const description = value?.description;
  if (description?.type !== type || typeof description.sdp !== 'string' ||
      !description.sdp.startsWith('v=0\r\n') || description.sdp.length > CODE_LIMIT) {
    throw new Error(`Expected an ${type} connection code`);
  }
  return { description: { type, sdp: description.sdp }, settings: validateSettings(value.settings) };
}

export function newSettings(delay, window, controller) {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  return validateSettings({ session: [...bytes].map(b => b.toString(16).padStart(2, '0')).join(''),
    delay, window, controller });
}

export class RtcLink {
  constructor({ iceServers = [], onOpen = () => {}, onClose = () => {},
    swapCallbacks = {},
    PeerConnection = globalThis.RTCPeerConnection } = {}) {
    if (!PeerConnection) throw new Error('This browser does not support WebRTC data channels');
    this.pc = new PeerConnection({ iceServers });
    this.channel = null;
    this.settings = null;
    this.incoming = [];
    this.closed = false;
    this.opened = false;
    this.onOpen = onOpen;
    this.onClose = onClose;
    this.timer = null;
    this.cancelGather = null;
    this.media = null;
    this.swaps = null;
    this.swapCallbacks = swapCallbacks;
    this.mediaReady = new Promise((resolve, reject) => { this.mediaAttached = resolve; this.mediaFailed = reject; });
    this.mediaReady.catch(() => {});
    this.diagnostics = new NetplayDiagnostics();
    this.diagnostics.record('created', this.pc);
    this.pc.ondatachannel = event => this.attach(event.channel);
    this.pc.onconnectionstatechange = () => {
      this.diagnostics.record('peer-state', this.pc);
      this.diagnostics.capture(this.pc);
      if (['failed', 'closed'].includes(this.pc.connectionState)) {
        this.close(connectionFailure(this.pc));
      }
    };
    this.pc.oniceconnectionstatechange = () => {
      this.diagnostics.record('ice-state', this.pc);
      this.diagnostics.capture(this.pc);
    };
    this.pc.onicegatheringstatechange = () => this.diagnostics.record('gathering-state', this.pc);
    this.pc.onsignalingstatechange = () => this.diagnostics.record('signaling-state', this.pc);
    this.pc.onicecandidateerror = event => {
      this.diagnostics.iceError(event.errorCode);
      this.diagnostics.record('ice-error', this.pc);
    };
  }

  attach(channel) {
    if (channel.label === SWAP_CHANNEL) {
      if (this.closed || this.swaps || this.settings?.swaps !== SWAP_VERSION ||
          channel.ordered !== true || channel.maxRetransmits != null || channel.maxPacketLifeTime != null) {
        channel.close();
        this.close('Unexpected disk swap channel. Refresh both pages.');
        return;
      }
      this.swaps = new DiskSwaps(channel, { ...this.swapCallbacks, host: this.host,
        fail: error => this.close(error.message) });
      return;
    }
    if (channel.label === MEDIA_CHANNEL) {
      if (this.closed || this.media || this.settings?.media !== MEDIA_VERSION ||
          channel.ordered !== true || channel.maxRetransmits != null || channel.maxPacketLifeTime != null) {
        channel.close();
        this.close('Unexpected game setup channel. Refresh both pages and start a new session.');
        return;
      }
      this.media = new MediaTransfer(channel, { host: this.host,
        fail: error => this.close(error.message) });
      this.mediaAttached(this.media);
      return;
    }
    if (this.closed || this.channel || channel.label !== CHANNEL ||
        channel.ordered || channel.maxRetransmits !== 0) {
      channel.close();
      this.close(`Unexpected netplay data channel: label=${channel.label}, ordered=${channel.ordered}, maxRetransmits=${channel.maxRetransmits}`);
      return;
    }
    this.channel = channel;
    channel.binaryType = 'arraybuffer';
    channel.onmessage = event => {
      if (this.closed) return;
      if (!(event.data instanceof ArrayBuffer) || event.data.byteLength > PACKET_LIMIT) {
        this.close('Invalid netplay packet');
        return;
      }
      // Browser timer throttling can batch valid retransmissions. Keep the
      // newest packets, which repeat every unacknowledged input.
      if (this.incoming.length === QUEUE_LIMIT) this.incoming.shift();
      this.incoming.push(new Uint8Array(event.data));
    };
    channel.onopen = () => {
      if (this.closed || this.opened) return;
      this.opened = true;
      this.diagnostics.record('data-open', this.pc);
      this.diagnostics.capture(this.pc);
      clearTimeout(this.timer);
      Promise.resolve().then(() => this.closed ? undefined : this.onOpen(this))
        .catch(error => { console.error('Netplay startup failed', error); this.close(String(error.message ?? error)); });
    };
    channel.onclose = () => {
      this.diagnostics.record('data-close', this.pc);
      this.close('Peer disconnected. Copy diagnostics for a connection report.');
    };
    channel.onerror = event => {
      this.diagnostics.record('data-error', this.pc);
      console.error('Netplay data channel failed', event.error ?? event);
      this.close(`Netplay data channel failed${event.error?.message ? ': ' + event.error.message : ''}`);
    };
  }

  async gather(description) {
    if (this.closed) throw new Error('Connection cancelled');
    await this.pc.setLocalDescription(description);
    if (this.closed) throw new Error('Connection cancelled');
    if (this.pc.iceGatheringState !== 'complete') {
      await new Promise((resolve, reject) => {
        let timer;
        const finish = error => {
          clearTimeout(timer);
          this.pc.removeEventListener('icegatheringstatechange', changed);
          this.cancelGather = null;
          error ? reject(error) : resolve();
        };
        const changed = () => {
          if (this.pc.iceGatheringState === 'complete') finish();
        };
        this.cancelGather = () => finish(new Error('Connection cancelled'));
        this.pc.addEventListener('icegatheringstatechange', changed);
        timer = setTimeout(() => {
          // One slow/unreachable ICE server must not discard usable routes
          // from the others. Later candidates may be omitted from this offer.
          if (/^a=candidate:/m.test(this.pc.localDescription?.sdp ?? '')) {
            this.diagnostics.record('gathering-deadline', this.pc);
            finish();
          } else finish(new Error('Network address discovery timed out without a usable route. Copy diagnostics, then try a new session.'));
        }, 15000);
        changed();
      });
    }
    if (this.closed) throw new Error('Connection cancelled');
    return encodeCode(this.pc.localDescription, this.settings);
  }

  configureIce(iceServers, relayOnly = false) {
    if (!Array.isArray(iceServers) || iceServers.length > 8) throw new Error('Invalid network configuration');
    if (relayOnly && !iceServers.some(server => [].concat(server.urls ?? []).some(url => /^turns?:/.test(url)))) {
      throw new Error('A relay is not available for this session');
    }
    const iceTransportPolicy = relayOnly ? 'relay' : 'all';
    try { this.pc.setConfiguration({ iceServers, iceTransportPolicy }); }
    catch (error) {
      if (error.name !== 'SyntaxError') throw error;
      // Some WebKit builds reject valid TURN transport queries. Retry using
      // default UDP for turn: and TLS/TCP for turns:, retaining ports and keys.
      // Plain TCP needs its query, so omit it only on this compatibility path.
      const compatible = iceServers.map(server => ({ ...server,
        urls: [].concat(server.urls ?? []).flatMap(url => {
          if (/^turn:[^?]+\?transport=udp$/i.test(url) || /^turns:[^?]+\?transport=tcp$/i.test(url)) return [url.split('?')[0]];
          if (/^turn:[^?]+\?transport=tcp$/i.test(url)) return [];
          return [url];
        }),
      })).filter(server => server.urls.length);
      if (JSON.stringify(compatible) === JSON.stringify(iceServers) ||
          !compatible.some(server => server.urls.some(url => /^turns?:/.test(url)))) throw error;
      this.pc.setConfiguration({ iceServers: compatible, iceTransportPolicy });
    }
  }

  report() { return this.diagnostics.report(this.pc, this.channel); }

  async offer(settings) {
    this.settings = validateSettings(settings);
    this.host = true;
    this.attach(this.pc.createDataChannel(CHANNEL, { ordered: false, maxRetransmits: 0 }));
    if (this.settings.media === MEDIA_VERSION) this.attach(this.pc.createDataChannel(MEDIA_CHANNEL, { ordered: true }));
    if (this.settings.swaps === SWAP_VERSION) this.attach(this.pc.createDataChannel(SWAP_CHANNEL, { ordered: true }));
    return this.gather(await this.pc.createOffer());
  }

  async answer(code) {
    const { description, settings } = decodeCode(code, 'offer');
    this.settings = settings;
    this.host = false;
    await this.pc.setRemoteDescription(description);
    if (this.closed) throw new Error('Connection cancelled');
    return this.gather(await this.pc.createAnswer());
  }

  async accept(code) {
    const { description, settings } = decodeCode(code, 'answer');
    if (JSON.stringify(settings) !== JSON.stringify(this.settings)) {
      throw new Error('Answer belongs to a different netplay session or version. Refresh both pages and try again.');
    }
    await this.pc.setRemoteDescription(description);
    if (this.closed) throw new Error('Connection cancelled');
    if (!this.opened) {
      clearTimeout(this.timer);
      this.timer = setTimeout(() => this.close('Peer connection timed out. Copy diagnostics, then start a new session.'), 60000);
    }
  }

  async transferMedia(snapshot, progress) {
    if (this.closed) throw new Error('Game setup transfer cancelled');
    this.timer = setTimeout(() => this.close('Game setup transfer timed out. Start a new session.'), 180000);
    try {
      const transfer = await this.mediaReady;
      transfer.progress = progress;
      if (this.host) await transfer.send(snapshot);
      else return await transfer.receive();
    } finally { clearTimeout(this.timer); }
  }

  receive(emu) {
    for (const packet of this.incoming.splice(0)) emu.netplay_receive(packet);
  }

  send(emu) {
    if (this.closed || this.channel?.readyState !== 'open') return;
    for (let count = 0; count < QUEUE_LIMIT && this.channel.bufferedAmount < PACKET_LIMIT * QUEUE_LIMIT; count++) {
      const packet = emu.netplay_take_packet();
      if (!packet.length) break;
      this.channel.send(packet);
    }
  }

  close(reason = 'Disconnected. Start a new session to play again.') {
    if (this.closed) return;
    this.closed = true;
    this.diagnostics.record('stopped', this.pc);
    this.diagnostics.capture(this.pc);
    clearTimeout(this.timer);
    this.cancelGather?.();
    this.mediaFailed(new Error('Game setup transfer cancelled'));
    // Let the owner poll final queued packets for the core's failure reason
    // before freeing the machine. A remote close can follow its hello packet.
    try { this.onClose(reason, this); }
    finally { this.dispose(); }
  }

  dispose() {
    if (this.swaps) {
      const channel = this.swaps.channel;
      channel.onmessage = channel.onclose = channel.onerror = null;
      this.swaps.stop();
      channel.close();
      this.swaps = null;
    }
    if (this.media) {
      const channel = this.media.channel;
      channel.onopen = channel.onmessage = channel.onclose = channel.onerror = null;
      this.media.stop();
      channel.close();
      this.media = null;
    }
    this.mediaReady = this.mediaAttached = this.mediaFailed = null;
    this.incoming.length = 0;
    this.pc.ondatachannel = this.pc.onconnectionstatechange = null;
    this.pc.oniceconnectionstatechange = this.pc.onicegatheringstatechange = null;
    this.pc.onsignalingstatechange = this.pc.onicecandidateerror = null;
    if (this.channel) {
      this.channel.onopen = this.channel.onmessage = this.channel.onclose = this.channel.onerror = null;
      this.channel.close();
    }
    this.pc.close();
  }
}

// The panel inserts itself into old static page shells, as the other controls do.
export function mountNetplayPanel(parent, { prepare, start, stop, getMedia, useMedia, getMachine, diskChanged }) {
  const style = document.createElement('style');
  style.textContent = `
    #netplay-panel { font-size: .88rem; line-height: 1.4; color: var(--ink-mute, #bbc0ca); }
    #netplay-panel summary { cursor: pointer; font-weight: 600; color: var(--ink, #eee); }
    #netplay-panel p { margin: .6rem 0; }
    #netplay-panel label { display: block; margin-top: .6rem; }
    #netplay-panel input, #netplay-panel textarea, #netplay-panel select {
      display: block; box-sizing: border-box; width: 100%; margin-top: .2rem;
      border: 1px solid var(--line, #454b57); border-radius: 6px; padding: .4rem;
      background: rgba(10, 13, 22, .6); color: var(--ink, #eee); font: inherit;
    }
    #netplay-panel textarea { resize: vertical; font-family: ui-monospace, monospace; font-size: .8rem; }
    #netplay-panel .btn { margin-top: .4rem; width: 100%; justify-content: center; font-size: .88rem; padding: .5rem; }
    #netplay-panel :disabled { opacity: .45; cursor: default; }
    #netplay-panel [hidden] { display: none !important; }
    #netplay-panel #netplay-status { overflow-wrap: anywhere; color: var(--ink, #eee); }
    #netplay-advanced { margin-top: .8rem; border-top: 1px solid var(--line, #454b57); padding-top: .6rem; }
    #netplay-qr { margin: .8rem auto; max-width: 260px; background: white; padding: .25rem; }
    #netplay-qr svg { display: block; width: 100%; height: auto; }
    #netplay-panel label:has(input[type=checkbox]) { display: flex; align-items: center; gap: .5rem; }
    #netplay-panel input[type=checkbox] { width: auto; margin: 0; }
    @media (pointer: coarse) {
      #netplay-panel input, #netplay-panel textarea, #netplay-panel select { font-size: 1rem; }
    }
  `;
  document.head.appendChild(style);
  const root = document.createElement('details');
  root.id = 'netplay-panel';
  root.className = 'try-side-section';
  root.innerHTML = `<summary>Netplay</summary>
    <p>The host shares their ROMs, disks and machine settings with player 2. Starting a session replaces your running game.</p>
    <div id="netplay-rooms">
      <button id="netplay-room-host" type="button">Host game</button>
      <label>Invitation link or room code <input id="netplay-room-code" autocomplete="off" autocapitalize="none" spellcheck="false"></label>
      <button id="netplay-room-join" type="button">Join game</button>
      <p id="netplay-service-status"></p>
    </div>
    <div id="netplay-invitation" hidden>
      <label>Your invitation <input id="netplay-invite" readonly spellcheck="false"></label>
      <button id="netplay-copy-invite" type="button">Copy invitation</button>
      <button id="netplay-share" type="button" hidden>Share invitation</button>
      <div id="netplay-qr" role="img" aria-label="Scan this QR code with the other device’s camera to join"></div>
      <p>Scan with the other device’s camera, or share the link. Invitations expire after 15 minutes.</p>
    </div>
    <button id="netplay-disconnect" type="button" disabled>Disconnect</button>
    <p id="netplay-status" role="status" aria-live="polite">Host a game or open an invitation to join.</p>
    <div id="netplay-disks" hidden>
      <p>The host can change disks here. Both players pause during the transfer and resume together.</p>
      <label>Drive <select id="netplay-disk-drive"><option value="0">DF0</option><option value="1">DF1</option></select></label>
      <label>Swap disk <input id="netplay-disk-file" type="file" accept=".adf,.adz,.dms,.ipf,.scp,.gz,.zip"></label>
      <label><input id="netplay-disk-writable" type="checkbox"> Allow writes to the replacement disk</label>
      <button id="netplay-disk-eject" type="button">Eject selected drive</button>
      <p>Writable changes stay in this session and are discarded when a disk is replaced or the session ends.</p>
    </div>
    <button id="netplay-diagnostics" type="button" disabled>Copy diagnostics</button>
    <textarea id="netplay-report" rows="5" readonly hidden aria-label="Connection diagnostics"></textarea>
    <details id="netplay-advanced"><summary>Advanced</summary>
      <label>Controllers <select id="netplay-controller"><option value="joystick">Joystick</option><option value="cd32">CD32 pad</option><option value="mouse">Two mice</option></select></label>
      <p>For two-mouse games, choose Two mice before hosting. Each player’s mouse or touch trackpad controls their own Amiga port.</p>
      <label>Input delay <select id="netplay-delay">${[0,1,2,3,4,5,6].map(n => `<option ${n === 2 ? 'selected' : ''}>${n}</option>`).join('')}</select></label>
      <label>Rollback limit <select id="netplay-window">${Array.from({length:12}, (_, i) => `<option ${i === 7 ? 'selected' : ''}>${i + 1}</option>`).join('')}</select></label>
      <label><input id="netplay-relay-only" type="checkbox"> Use relay only for room connections</label>
      <p>Room connections try a direct route and can use a relay automatically. Relay-only mode helps diagnose connection problems.</p>
      <h4>Manual connection codes</h4>
      <label>STUN server <input id="netplay-stun" value="stun:stun.l.google.com:19302" spellcheck="false"></label>
      <p>Leave STUN blank for LAN-only discovery. Manual setup has no relay fallback.</p>
      <button id="netplay-host" type="button">Host with manual codes</button>
      <label>Code from the other player <textarea id="netplay-remote" rows="3" spellcheck="false"></textarea></label>
      <button id="netplay-join" type="button">Join offer</button>
      <button id="netplay-accept" type="button" disabled>Connect answer</button>
      <label>Your connection code <textarea id="netplay-local" rows="3" readonly spellcheck="false"></textarea></label>
      <button id="netplay-copy" type="button" disabled>Copy code</button>
    </details>`;
  for (const button of root.querySelectorAll('button')) button.className = 'btn btn--ghost';
  root.addEventListener('keydown', event => event.stopPropagation());
  root.addEventListener('keyup', event => event.stopPropagation());
  parent.insertBefore(root, parent.querySelector('.try-side-section'));
  const field = name => root.querySelector(`#netplay-${name}`);
  const service = document.querySelector('meta[name="copperline-netplay-service"]')?.content?.trim();
  let link = null;
  let lastLink = null;
  let readingDisk = false;
  const status = text => { field('status').textContent = text; };
  const controls = () => {
    const active = !!link;
    for (const name of ['host', 'join', 'delay', 'window', 'controller', 'stun', 'relay-only', 'room-code']) field(name).disabled = active;
    for (const name of ['room-host', 'room-join']) field(name).disabled = active || !service;
    field('disconnect').disabled = !active;
    field('copy').disabled = !field('local').value;
    field('diagnostics').disabled = !lastLink;
    const swapEnabled = !!link?.host && link.settings?.swaps === SWAP_VERSION;
    field('disks').hidden = !swapEnabled;
    const canSwap = swapEnabled && link.swaps?.channel.readyState === 'open'
      && !!getMachine?.(link)?.netplay_status()[0] && !link.swaps.busy && !readingDisk;
    for (const name of ['disk-drive', 'disk-file', 'disk-writable', 'disk-eject']) field(name).disabled = !canSwap;
    if (!active) field('accept').disabled = true;
  };
  field('service-status').textContent = service
    ? 'Host is player 1; Join is player 2. Received files are used only for this session.'
    : 'Room invitations are not configured on this page. Manual setup is available under Advanced.';
  field('advanced').open = !service;
  field('share').hidden = typeof navigator.share !== 'function';

  function readInvitation() {
    if (link) return;
    const room = new URLSearchParams(location.hash.slice(1)).get('room');
    if (!room) return;
    if (!roomFromInvite(room)) { root.open = true; status('This invitation is incomplete or damaged.'); return; }
    field('room-code').value = room;
    root.open = true;
    status('Invitation ready. Click Join game to receive the host’s files and machine settings.');
  }
  readInvitation();
  window.addEventListener('hashchange', readInvitation);

  async function begin(mode) {
    if (link) return;
    const host = mode.endsWith('host');
    const roomMode = mode.startsWith('room-');
    let current;
    let settings;
    try {
      const remote = field('remote').value;
      const roomId = roomFromInvite(field('room-code').value);
      if (roomMode && !service) throw new Error('Room invitations are not configured on this page');
      if (roomMode && !host && !roomId) throw new Error('Paste an invitation link or room code');
      settings = host ? { ...newSettings(Number(field('delay').value), Number(field('window').value), field('controller').value), media: MEDIA_VERSION, swaps: SWAP_VERSION }
        : roomMode ? null : decodeCode(remote, 'offer').settings;
      const stun = field('stun').value.trim();
      if (!roomMode && stun && !/^stuns?:[^\s]+$/i.test(stun)) throw new Error('STUN server must start with stun: or stuns:');
      current = new RtcLink({ iceServers: !roomMode && stun ? [{ urls: stun }] : [],
        swapCallbacks: {
          machine: () => getMachine(current), status,
          changed: disk => { if (link === current) { if (disk) diskChanged(current, disk); controls(); } },
        },
        onOpen: async peer => {
          if (link !== peer) return;
          if (settings.media === MEDIA_VERSION) {
            status(host ? 'Sending game setup...' : 'Receiving the host’s game setup...');
            const received = await peer.transferMedia(host ? getMedia(peer) : null, (action, bytes, total) => {
              if (link === peer) status(`${action} game setup: ${Math.floor(bytes * 100 / total)}%`);
            });
            if (link !== peer) return;
            if (!host) useMedia(peer, received);
          }
          status('Connected. Checking the initial machines...');
          await start(peer, settings, host ? 1 : 2);
        },
        onClose: (reason, peer) => {
          if (link !== peer) return;
          peer.abort.abort();
          peer.room?.end();
          link = null;
          field('local').value = '';
          field('invite').value = '';
          field('qr').replaceChildren();
          field('invitation').hidden = true;
          controls();
          status(stop(reason, peer) ?? reason);
        },
      });
      current.abort = new AbortController();
      link = lastLink = current;
      field('local').value = '';
      field('report').hidden = true;
      controls();
      status('Preparing a fresh session...');
      await prepare(current, { host, receiveMedia: !host && (roomMode || settings?.media === MEDIA_VERSION) });
      if (link !== current) return;
      if (roomMode) {
        current.room = new RoomClient(service, current.abort.signal);
        status(host ? 'Creating your invitation...' : 'Joining the room...');
        const network = host ? await current.room.create() : await current.room.join(roomId);
        if (link !== current) { current.room.end(); return; }
        if (!Number.isFinite(network.expiresAt) || network.expiresAt <= Date.now()) throw new Error('The invitation has expired');
        if (!host) settings = decodeCode(network.offer, 'offer').settings;
        current.configureIce(network.iceServers, field('relay-only').checked);
        status('Finding a connection route...');
        const code = host ? await current.offer(settings) : await current.answer(network.offer);
        if (link !== current) return;
        await current.room.publish(host ? 'offer' : 'answer', code);
        if (link !== current) return;
        if (host) {
          const invitation = inviteUrl(current.room.id);
          field('invite').value = invitation;
          const qr = qrcode(0, 'M');
          qr.addData(invitation);
          qr.make();
          field('qr').innerHTML = qr.createSvgTag({ cellSize: 4, margin: 16, scalable: true });
          field('invitation').hidden = false;
          status('Waiting for player 2. Share the invitation or scan the QR code.');
          const answer = await current.room.waitForAnswer(network.expiresAt);
          if (link !== current) return;
          await current.accept(answer);
        } else if (!current.opened) {
          current.timer = setTimeout(() => current.close('Connection timed out. Copy diagnostics, then start a new room.'), 60000);
        }
        if (link === current && !current.opened) status('Connecting to the other player...');
      } else {
        status('Gathering network addresses...');
        const code = host ? await current.offer(settings) : await current.answer(remote);
        if (link !== current) return;
        field('local').value = code;
        field('copy').disabled = false;
        field('accept').disabled = !host;
        status(host ? 'Send your offer code. Paste the reply and click Connect answer.' : 'Send your answer code back to the host. Keep this page open, or Disconnect to cancel.');
      }
    } catch (error) {
      if (current && link === current) current.close(String(error.message ?? error));
      else if (!current) status(String(error.message ?? error));
    }
  }
  field('host').addEventListener('click', () => begin('manual-host'));
  field('join').addEventListener('click', () => begin('manual-join'));
  field('room-host').addEventListener('click', () => begin('room-host'));
  field('room-join').addEventListener('click', () => begin('room-join'));
  field('disk-file').addEventListener('change', async () => {
    const current = link;
    const file = field('disk-file').files?.[0];
    if (!file || !current?.host || !current.swaps || readingDisk) return;
    const drive = Number(field('disk-drive').value);
    const writable = field('disk-writable').checked;
    readingDisk = true;
    controls();
    try {
      if (!file.size || file.size > DISK_LIMIT) throw new Error('Select a disk image of up to 16 MiB');
      const bytes = new Uint8Array(await file.arrayBuffer());
      if (link !== current) return;
      await current.swaps.swap(drive, { bytes, name: file.name, writable });
    } catch (error) { if (link === current) status(String(error.message ?? error)); }
    finally { readingDisk = false; field('disk-file').value = ''; controls(); }
  });
  field('disk-eject').addEventListener('click', async () => {
    const current = link;
    if (!current?.host || !current.swaps || readingDisk) return;
    try { await current.swaps.swap(Number(field('disk-drive').value), null); }
    catch (error) { if (link === current) status(String(error.message ?? error)); }
  });
  field('accept').addEventListener('click', async () => {
    const current = link;
    if (!current) return;
    field('accept').disabled = true;
    try {
      await current.accept(field('remote').value);
      if (link !== current) return;
      if (!current.opened) status('Connecting to the other player...');
    } catch (error) {
      if (link !== current) return;
      field('accept').disabled = current.opened;
      status(String(error.message ?? error));
    }
  });
  async function copy(name, message) {
    try { await navigator.clipboard.writeText(field(name).value); status(message); }
    catch { field(name).hidden = false; field(name).focus(); field(name).select(); status('Copy the selected text'); }
  }
  field('copy').addEventListener('click', () => copy('local', 'Connection code copied'));
  field('copy-invite').addEventListener('click', () => copy('invite', 'Invitation copied'));
  field('share').addEventListener('click', async () => {
    try { await navigator.share({ title: 'Join my Copperline game', url: field('invite').value }); }
    catch (error) { if (error.name !== 'AbortError') copy('invite', 'Invitation copied'); }
  });
  field('diagnostics').addEventListener('click', async () => {
    const peer = lastLink;
    if (!peer) return;
    field('report').value = JSON.stringify(await peer.report(), null, 2);
    await copy('report', 'Diagnostics copied. The report excludes connection codes, credentials and network addresses.');
  });
  field('disconnect').addEventListener('click', () => link?.close());
  window.addEventListener('pagehide', () => link?.close());
  controls();
  return { get link() { return link; }, status: text => { controls(); if (!link?.swaps?.busy) status(text); }, root };
}
