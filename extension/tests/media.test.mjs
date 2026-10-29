// Spotting downloadable video and choosing how to fetch it.
// Run with: node --test "extension/tests/*.test.mjs"

import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const read = (name) => fs.readFileSync(new URL(`../${name}`, import.meta.url), 'utf8');
const context = vm.createContext({ chrome: {}, URL });
vm.runInContext(`${read('common.js')}\n${read('media.js')}
;globalThis.api = { classifyResponse, videoChoices, videoPageUrl, readHlsPlaylist, cleanTitle };`, context);
const { classifyResponse, videoChoices, videoPageUrl, readHlsPlaylist, cleanTitle } = context.api;
// Objects from the sandbox have its own prototypes, so compare plain copies.
const plain = (v) => JSON.parse(JSON.stringify(v));

const response = (url, type, mime, extra = {}) => ({
  url,
  type,
  statusCode: 200,
  responseHeaders: [{ name: 'Content-Type', value: mime }, ...Object.entries(extra).map(([name, value]) => ({ name, value }))],
});

test('manifests count however they were fetched', () => {
  assert.equal(classifyResponse(response('https://cdn.x/a/master.m3u8', 'xmlhttprequest', 'text/plain')).kind, 'hls');
  assert.equal(classifyResponse(response('https://cdn.x/play', 'xmlhttprequest', 'application/vnd.apple.mpegurl')).kind, 'hls');
  assert.equal(classifyResponse(response('https://cdn.x/stream.mpd?t=1', 'xmlhttprequest', 'application/dash+xml')).kind, 'dash');
});

test('whole files count only when a media element loaded them', () => {
  const big = { 'Content-Range': 'bytes 0-1023/52428800' };
  const file = classifyResponse(response('https://cdn.x/movie.mp4', 'media', 'video/mp4', big));
  assert.equal(file.kind, 'file');
  assert.equal(file.size, 52428800);
  // Pieces a player fetches itself, stream segments and tiny previews don't.
  assert.equal(classifyResponse(response('https://cdn.x/videoplayback?range=0-9', 'xmlhttprequest', 'video/mp4', big)), null);
  assert.equal(classifyResponse(response('https://cdn.x/seg1.ts', 'media', 'video/mp2t', big)), null);
  assert.equal(classifyResponse(response('https://cdn.x/preview.mp4', 'media', 'video/mp4', { 'Content-Length': '90000' })), null);
  assert.equal(classifyResponse(response('https://cdn.x/page', 'main_frame', 'text/html')), null);
  assert.equal(classifyResponse({ ...response('https://cdn.x/gone.mp4', 'media', 'video/mp4'), statusCode: 404 }), null);
});

test('video sites go to yt-dlp, with YouTube links reduced to the video', () => {
  assert.equal(videoPageUrl('https://www.youtube.com/watch?v=abc123&list=PL1&t=40s'), 'https://www.youtube.com/watch?v=abc123');
  assert.equal(videoPageUrl('https://www.youtube.com/embed/abc123?autoplay=1'), 'https://www.youtube.com/watch?v=abc123');
  assert.equal(videoPageUrl('https://youtu.be/abc123'), 'https://www.youtube.com/watch?v=abc123');
  assert.equal(videoPageUrl('https://www.youtube.com/'), null, 'the home page is not a video');
  assert.equal(videoPageUrl('https://vimeo.com/76979871#t=10'), 'https://vimeo.com/76979871');
  assert.equal(videoPageUrl('https://news.example.com/story'), null);

  const choices = videoChoices({ frameUrl: 'https://www.youtube.com/embed/abc123', pageUrl: 'https://news.example.com/story', frameId: 4, src: 'blob:https://www.youtube.com/1' });
  assert.deepEqual(plain(choices.map((c) => [c.kind, c.url])), [['video', 'https://www.youtube.com/watch?v=abc123']]);
});

test('a plain video file is downloaded as what the element plays', () => {
  const media = [{ kind: 'file', url: 'https://cdn.x/clip.mp4', mime: 'video/mp4', size: 7340032, frameId: 0 }];
  const [first] = videoChoices({ frameUrl: 'https://site.example/post', pageUrl: 'https://site.example/post', src: 'https://cdn.x/clip.mp4', media });
  assert.equal(first.kind, 'file');
  assert.equal(first.url, 'https://cdn.x/clip.mp4');
  assert.equal(first.label, 'MP4 · 7.0 MB · cdn.x');
});

test('players fed in pieces use the streams the network showed', () => {
  const media = [
    { kind: 'hls', url: 'https://ads.x/ad/master.m3u8', frameId: 0 },
    { kind: 'file', url: 'https://cdn.x/teaser.webm', mime: 'video/webm', size: 9e6, frameId: 0 },
    { kind: 'hls', url: 'https://cdn.x/show/master.m3u8', frameId: 0 },
    { kind: 'hls', url: 'https://cdn.x/show/720p.m3u8', frameId: 0 },
    { kind: 'dash', url: 'https://other.x/s.mpd', frameId: 7 },
  ];
  const hls = {
    'https://ads.x/ad/master.m3u8': { master: true, height: 480 },
    'https://cdn.x/show/master.m3u8': { master: true, height: 1080 },
    'https://cdn.x/show/720p.m3u8': { master: false, height: null },
  };
  const choices = videoChoices({ frameUrl: 'https://site.example/watch', pageUrl: 'https://site.example/watch', frameId: 0, src: 'blob:https://site.example/9', media, hls });
  assert.deepEqual(plain(choices.map((c) => c.url)), [
    'https://cdn.x/show/master.m3u8', // newest master in this frame
    'https://ads.x/ad/master.m3u8',
    'https://cdn.x/show/720p.m3u8', // a single quality
    'https://cdn.x/teaser.webm',
    'https://other.x/s.mpd', // another frame's, last
  ]);
  assert.equal(choices[0].label, 'HLS stream · up to 1080p · cdn.x');
  assert.equal(choices[4].kind, 'video', 'DASH goes through yt-dlp');
});

test('with nothing seen, yt-dlp looks at the page', () => {
  const choices = videoChoices({ frameUrl: 'https://site.example/v/1', pageUrl: 'https://site.example/v/1', src: 'blob:https://site.example/1' });
  assert.deepEqual(plain(choices.map((c) => [c.kind, c.url])), [['video', 'https://site.example/v/1']]);
});

test('reading HLS playlists and titles', () => {
  assert.deepEqual(plain(readHlsPlaylist('#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,RESOLUTION=640x360\na.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2,RESOLUTION=1920x1080\nb.m3u8\n')), { master: true, height: 1080 });
  assert.deepEqual(plain(readHlsPlaylist('#EXTM3U\n#EXTINF:4,\ns1.ts\n')), { master: false, height: null });
  assert.equal(cleanTitle('(3) Big Buck Bunny - YouTube'), 'Big Buck Bunny');
  assert.equal(cleanTitle('Launch day | The Verge'), 'Launch day');
  assert.equal(cleanTitle('Just a title'), 'Just a title');
});
