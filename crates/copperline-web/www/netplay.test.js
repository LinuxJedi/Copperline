// SPDX-License-Identifier: GPL-3.0-or-later
import assert from 'node:assert/strict';
import test from 'node:test';
import { RtcLink, decodeCode, encodeCode, newSettings, validateSettings } from './netplay.js';

const settings = { session: '0123456789abcdef0123456789abcdef', delay: 2, window: 8, controller: 'joystick' };
const description = type => ({ type, sdp: 'v=0\r\ns=-\r\n' });

class Channel {
  constructor(label, options) { Object.assign(this, { label, ...options, bufferedAmount: 0, readyState: 'connecting', sent: [] }); }
  send(packet) { this.sent.push(packet); }
  close() { this.readyState = 'closed'; }
}
class Peer extends EventTarget {
  constructor() { super(); this.iceGatheringState = 'complete'; this.connectionState = 'new'; }
  createDataChannel(label, options) { return new Channel(label, options); }
  async createOffer() { return description('offer'); }
  async createAnswer() { return description('answer'); }
  async setLocalDescription(value) { this.localDescription = value; }
  async setRemoteDescription(value) { this.remoteDescription = value; }
  close() { this.connectionState = 'closed'; }
}

test('connection codes round-trip only valid settings and the expected description type', () => {
  const code = encodeCode(description('offer'), settings);
  assert.deepEqual(decodeCode(code, 'offer'), { description: description('offer'), settings });
  assert.throws(() => decodeCode(code, 'answer'), /Expected/);
  for (const bad of ['', 'CLNP1.!', 'CLNP1.' + 'A'.repeat(100000)]) assert.throws(() => decodeCode(bad, 'offer'));
  for (const key of ['delay', 'window']) {
    for (const value of [-1, 1.5, 257, NaN, Infinity, '2']) assert.throws(() => validateSettings({ ...settings, [key]: value }));
  }
  assert.throws(() => validateSettings({ ...settings, controller: 'analogue' }));
  assert.throws(() => validateSettings({ ...settings, session: 'bad' }));
  assert.notEqual(newSettings(0, 1, 'cd32').session, newSettings(0, 1, 'cd32').session);
  const shared = { ...settings, media: 'host-v1', swaps: 'disk-v1' };
  assert.deepEqual(decodeCode(encodeCode(description('offer'), shared), 'offer').settings, shared);
  assert.throws(() => validateSettings({ ...settings, media: 'unknown' }));
  assert.throws(() => validateSettings({ ...settings, swaps: 'unknown' }));
});

test('setup-enabled offers create a reliable channel and reject incompatible setup channels', async () => {
  const link = new RtcLink({ PeerConnection: Peer });
  await link.offer({ ...settings, media: 'host-v1', swaps: 'disk-v1' });
  assert.equal(link.media.channel.label, 'copperline-setup-v1');
  assert.equal(link.media.channel.ordered, true);
  assert.equal(link.media.channel.maxRetransmits, undefined);
  assert.equal(link.swaps.channel.label, 'copperline-disks-v1');
  assert.equal(link.swaps.channel.ordered, true);
  assert.equal(link.swaps.channel.maxRetransmits, undefined);
  link.close();
  assert.equal(link.media, null);
  assert.equal(link.mediaReady, null);
  assert.equal(link.swaps, null);
  const other = new RtcLink({ PeerConnection: Peer });
  other.attach(new Channel('copperline-setup-v1', { ordered: false, maxRetransmits: 0 }));
  assert.equal(other.closed, true);
});

test('host accepts only its answer and negotiates an unordered channel without retransmission', async () => {
  const host = new RtcLink({ PeerConnection: Peer });
  const join = new RtcLink({ PeerConnection: Peer });
  try {
    const offer = await host.offer(settings);
    const answer = await join.answer(offer);
    await assert.rejects(host.accept(encodeCode(description('answer'), { ...settings, delay: 3 })), /different/);
    assert.equal(host.pc.remoteDescription, undefined);
    await host.accept(answer);
    assert.equal(host.channel.ordered, false);
    assert.equal(host.channel.maxRetransmits, 0);
    assert.equal(host.channel.binaryType, 'arraybuffer');
    assert.deepEqual(join.settings, settings);
  } finally { host.close(); join.close(); }
});

test('cancelling ICE gathering rejects the pending offer and closes only once', async () => {
  let closed = 0;
  const link = new RtcLink({ PeerConnection: Peer, onClose: () => closed++ });
  link.pc.iceGatheringState = 'gathering';
  const offer = link.offer(settings);
  await new Promise(resolve => setImmediate(resolve));
  link.close();
  link.close();
  await assert.rejects(offer, /cancelled/);
  assert.equal(closed, 1);
  assert.equal(link.pc.connectionState, 'closed');
});

test('the gathering deadline preserves collected routes but rejects an empty offer', async t => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  for (const candidate of ['', 'a=candidate:1 1 udp 1 192.0.2.1 3478 typ relay\r\n']) {
    const link = new RtcLink({ PeerConnection: Peer });
    link.pc.iceGatheringState = 'gathering';
    link.pc.createOffer = async () => ({ type: 'offer', sdp: 'v=0\r\n' + candidate });
    const offer = link.offer(settings);
    await new Promise(resolve => setImmediate(resolve));
    t.mock.timers.tick(15000);
    if (candidate) {
      assert.ok(decodeCode(await offer, 'offer').description.sdp.includes(candidate));
      assert.equal(link.closed, false);
      assert.equal(link.diagnostics.events.at(-1).event, 'gathering-deadline');
    } else await assert.rejects(offer, /without a usable route/);
    link.close();
  }
});

test('a delayed offer cannot resurrect a cancelled link', async () => {
  const link = new RtcLink({ PeerConnection: Peer });
  let finish;
  link.pc.createOffer = () => new Promise(resolve => { finish = resolve; });
  const offer = link.offer(settings);
  link.close();
  finish(description('offer'));
  await assert.rejects(offer, /cancelled/);
  assert.equal(link.pc.localDescription, undefined);
});

test('packet queues bound throttled-browser bursts and respect send backpressure', async () => {
  const link = new RtcLink({ PeerConnection: Peer });
  await link.offer(settings);
  const channel = link.channel;
  channel.readyState = 'open';
  for (let i = 0; i < 100; i++) channel.onmessage({ data: new Uint8Array([i]).buffer });
  const received = [];
  link.receive({ netplay_receive: bytes => received.push(bytes[0]) });
  assert.equal(received.length, 64);
  assert.equal(received[0], 36);
  assert.equal(received.at(-1), 99);
  let drained = 0;
  const emu = { netplay_take_packet: () => ++drained < 3 ? new Uint8Array([1]) : new Uint8Array() };
  channel.bufferedAmount = 1103 * 64;
  link.send(emu);
  assert.equal(drained, 0);
  channel.bufferedAmount = 0;
  link.send(emu);
  assert.equal(channel.sent.length, 2);
  channel.onmessage({ data: new ArrayBuffer(1104) });
  assert.equal(link.closed, true);
});

test('duplicate or incompatible channels close without running the emulator', () => {
  let opened = 0;
  const link = new RtcLink({ PeerConnection: Peer, onOpen: () => opened++ });
  link.attach(new Channel('copperline-netplay-v1', { ordered: true, maxRetransmits: null }));
  assert.equal(link.closed, true);
  assert.equal(opened, 0);
});

test('data-channel open runs once, remote close drains queued input before cleanup', async () => {
  let opens = 0;
  let closed;
  const link = new RtcLink({ PeerConnection: Peer, onOpen: () => opens++,
    onClose: (reason, peer) => {
      const received = [];
      peer.receive({ netplay_receive: packet => received.push(...packet) });
      closed = { reason, received };
    } });
  const channel = new Channel('copperline-netplay-v1', { ordered: false, maxRetransmits: 0 });
  link.pc.ondatachannel({ channel });
  channel.onopen();
  channel.onopen();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(opens, 1);
  channel.onmessage({ data: new Uint8Array([7, 8]).buffer });
  channel.onclose();
  assert.deepEqual(closed, { reason: 'Peer disconnected. Copy diagnostics for a connection report.', received: [7, 8] });
  assert.equal(link.closed, true);
  assert.equal(channel.onopen, null);
  assert.equal(link.pc.ondatachannel, null);
  assert.equal(link.incoming.length, 0);
});

test('connection-state failure closes once and an open queued before cancellation stays cancelled', async () => {
  let opens = 0, closes = 0;
  const link = new RtcLink({ PeerConnection: Peer, onOpen: () => opens++, onClose: () => closes++ });
  await link.offer(settings);
  link.channel.onopen();
  link.pc.connectionState = 'failed';
  link.pc.onconnectionstatechange();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(opens, 0);
  assert.equal(closes, 1);
  assert.equal(link.channel.readyState, 'closed');
});


test('relay-only configuration requires TURN and preserves the requested ICE policy', () => {
  const link = new RtcLink({ PeerConnection: Peer });
  let config;
  link.pc.setConfiguration = value => { config = value; };
  try {
    assert.throws(() => link.configureIce([], true), /relay is not available/);
    const servers = [{ urls: ['stun:example.test', 'turns:example.test:443'], username: 'temporary', credential: 'temporary' }];
    link.configureIce(servers, true);
    assert.deepEqual(config, { iceServers: servers, iceTransportPolicy: 'relay' });
    link.configureIce(servers);
    assert.equal(config.iceTransportPolicy, 'all');
  } finally { link.close(); }
});

test('TURN query compatibility preserves UDP, TLS, credentials and relay-only policy', () => {
  const link = new RtcLink({ PeerConnection: Peer });
  const configurations = [];
  link.pc.setConfiguration = value => {
    configurations.push(value);
    if (value.iceServers.some(server => [].concat(server.urls).some(url => url.includes('?')))) throw new DOMException('Invalid TURN URL query string', 'SyntaxError');
  };
  try {
    const credentials = { username: 'temporary-user', credential: 'temporary-key' };
    const servers = [{ urls: 'stun:example.test:3478' }, { ...credentials, urls: [
      'turn:example.test:3478?transport=udp', 'turn:example.test:3478?transport=tcp',
      'turns:example.test:443?transport=tcp', 'turns:example.test:5349',
    ] }];
    link.configureIce(servers, true);
    assert.equal(configurations.length, 2);
    assert.equal(configurations[0].iceServers, servers);
    assert.deepEqual(configurations[1], { iceTransportPolicy: 'relay', iceServers: [
      { urls: ['stun:example.test:3478'] },
      { ...credentials, urls: ['turn:example.test:3478', 'turns:example.test:443', 'turns:example.test:5349'] },
    ] });
    assert.throws(() => link.configureIce([{ ...credentials, urls: 'turn:example.test:80?transport=tcp' }], true), { name: 'SyntaxError' });
    link.pc.setConfiguration = () => { throw new DOMException('credentials invalid', 'InvalidAccessError'); };
    assert.throws(() => link.configureIce(servers, true), { name: 'InvalidAccessError' });
  } finally { link.close(); }
});

test('two-mouse controller settings survive signaling', () => {
  const mice = { ...settings, controller: 'mouse' };
  assert.deepEqual(validateSettings(mice), mice);
  const description = { type: 'offer', sdp: 'v=0\r\n' };
  assert.deepEqual(decodeCode(encodeCode(description, mice), 'offer').settings, mice);
});
