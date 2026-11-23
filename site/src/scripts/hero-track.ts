// The hero's 3D running track: one fragment shader over the hero, no library.
// Each pixel casts a ray onto a ground plane. On the plane: a stadium track
// with four lanes (Android, iOS, Web, API) and a glossy runner in each, and
// around it a halftone dot field that swells under the pointer and lights in
// brand orange as pulses spread out from the runners. Hovering a lane, or a
// target chip above the headline, lights that lane and speeds its runner.

const VERT = 'attribute vec2 a;void main(){gl_Position=vec4(a,0.,1.);}';

const FRAG = `#extension GL_OES_standard_derivatives : enable
precision highp float;
uniform vec2 uRes;      // CSS px
uniform float uDpr;
uniform float uT;
uniform float uDark;
uniform vec3 uCam;      // track centre on screen (CSS px from top-left), scale
uniform vec2 uView;     // yaw, pitch
uniform vec2 uMask;     // copy edge: x (desktop, text on the left) or y (narrow, text above)
uniform vec3 uPtr;      // pointer x, y, on
uniform vec4 uRun;      // each lane's runner, as a fraction of a lap
uniform vec4 uLit;      // each lane's highlight, 0..1
uniform sampler2D uIcons; // the four lane icons side by side, white on transparent

const float A = .9;     // half the straight
const float R0 = 1.;    // inner radius
const float LW = .17;   // lane width
const float FOC = 1.6;
const float DIST = 5.4;
const float PI = 3.14159265;
const vec3 BRAND = vec3(1., .416, .239);

float h21(vec2 p){ p = fract(p * vec2(233.34, 851.73)); p += dot(p, p + 23.45); return fract(p.x * p.y); }
float vn(vec2 p){ vec2 i = floor(p), f = fract(p); f = f * f * (3. - 2. * f);
  return mix(mix(h21(i), h21(i + vec2(1., 0.)), f.x), mix(h21(i + vec2(0., 1.)), h21(i + vec2(1., 1.)), f.x), f.y); }

float sdTrack(vec2 q){ return length(vec2(max(abs(q.x) - A, 0.), q.y)) - R0; }
vec2 gradTrack(vec2 q){ vec2 v = vec2(sign(q.x) * max(abs(q.x) - A, 0.), q.y); float l = length(v); return l > 1e-4 ? v / l : vec2(0., 1.); }
float perim(float r){ return 4. * A + 2. * PI * r; }
// Where on the lap a point is, 0..1, counter-clockwise from the near straight.
float lapU(vec2 q, float r){
  float s;
  if (abs(q.x) <= A) s = q.y < 0. ? q.x + A : 2. * A + PI * r + (A - q.x);
  else if (q.x > 0.) s = 2. * A + (atan(q.y, q.x - A) + PI * .5) * r;
  else { float th = atan(q.y, q.x + A); if (th < 0.) th += 2. * PI; s = 4. * A + PI * r + (th - PI * .5) * r; }
  return s / perim(r);
}
vec2 lapPoint(float u, float r){
  float s = fract(u) * perim(r);
  if (s < 2. * A) return vec2(s - A, -r);
  s -= 2. * A;
  if (s < PI * r) { float th = s / r - PI * .5; return vec2(A + r * cos(th), r * sin(th)); }
  s -= PI * r;
  if (s < 2. * A) return vec2(A - s, r);
  s -= 2. * A;
  float th = PI * .5 + s / r; return vec2(-A + r * cos(th), r * sin(th));
}
float pick(vec4 v, float i){ return i < .5 ? v.x : i < 1.5 ? v.y : i < 2.5 ? v.z : v.w; }

vec4 over(vec4 top, vec4 under){ return top + under * (1. - top.a); }

void main(){
  vec2 p = vec2(gl_FragCoord.x, uRes.y * uDpr - gl_FragCoord.y) / uDpr;
  float dark = uDark;

  // Camera on a sphere around the track's centre, looking at it.
  float yaw = uView.x, pitch = uView.y;
  vec3 eye = DIST * vec3(sin(yaw) * cos(pitch), sin(pitch), -cos(yaw) * cos(pitch));
  vec3 f = normalize(-eye);
  vec3 rt = normalize(cross(vec3(0., 1., 0.), f));
  vec3 up = cross(f, rt);
  vec2 uv = vec2(p.x - uCam.x, uCam.y - p.y) / uCam.z;
  vec3 ray = normalize(f * FOC + rt * uv.x + up * uv.y);

  // Where the copy runs, everything stays faint.
  float copy = 1.;
  if (uMask.x > 0.) copy = mix(.13, 1., smoothstep(uMask.x - 40., uMask.x + 160., p.x));
  else if (uMask.y > 0.) copy = mix(.12, 1., smoothstep(uMask.y - 30., uMask.y + 70., p.y));
  float edge = smoothstep(0., 70., uRes.y - p.y) * smoothstep(0., 40., p.y);

  vec4 ground = vec4(0.);
  if (ray.y < -1e-3) {
    float t = -eye.y / ray.y;
    vec2 q = (eye + t * ray).xz;
    float fog = smoothstep(17., 6., t);
    float d = sdTrack(q);
    float aa = max(fwidth(d), 1e-4);
    float outer = 4. * LW;

    // Dots: on the ground outside the track and in the infield.
    float cell = .15;
    vec2 id = floor(q / cell), c = (id + .5) * cell;
    float patch_ = smoothstep(.36, .74, vn(id * .07 + vec2(uT * .018, 3.)));
    float fine = vn(id * .37 + 5.);
    float rad = cell * (.06 + .2 * patch_ * fine);
    float near = uPtr.z * exp(-dot(p - uPtr.xy, p - uPtr.xy) / 11000.);
    rad += cell * .14 * near;
    // A few dots light and go out again, each cell on its own clock.
    float clock = uT * .24 + h21(id + vec2(3.1, 7.7)) * 9.;
    float lit = smoothstep(.94, .985, h21(id * 1.37 + floor(clock))) * pow(sin(fract(clock) * PI), 2.);
    // And a ring of them spreads from each runner as it passes the line.
    for (int k = 0; k < 4; k++) {
      float fk = float(k);
      vec2 hk = lapPoint(pick(uRun, fk), R0 + (fk + .5) * LW);
      float ph = fract(uT * .16 + fk * .25);
      float ring = exp(-pow((length(c - hk) - ph * 5.) * 3.2, 2.)) * (1. - ph);
      lit = max(lit, ring * smoothstep(.35, .7, fine) * (.55 + .45 * pick(uLit, fk)));
    }
    lit *= smoothstep(.2, .55, patch_ + .25);
    rad += cell * .07 * lit;
    float dd = length(q - c);
    float daa = max(fwidth(dd), 1e-4);
    float dotA = 1. - smoothstep(rad - daa, rad + daa, dd);
    float clear = max(smoothstep(outer + .05, outer + .2, d), smoothstep(-.06, -.2, d));
    float o = (dark > .5 ? .36 : .42) * (.3 + .7 * patch_ + near) * dotA * clear;
    float lo = mix(o, (dark > .5 ? .95 : .85) * dotA * clear, lit);
    vec3 dcol = mix(dark > .5 ? vec3(.66, .7, .77) : vec3(.33, .37, .45), dark > .5 ? BRAND : vec3(.88, .32, .13), lit);
    lo *= fog * copy * edge;
    ground = vec4(dcol * lo, lo);

    // The track: four bevelled lanes with painted lines and a curb.
    if (d > -.02 && d < outer + .05) {
      float k = clamp(floor(d / LW), 0., 3.);
      float tt = clamp((d - (k + .5) * LW) / (LW * .5), -1., 1.);
      float hi = pick(uLit, k);
      vec2 g = gradTrack(q);
      float dh = -sin(tt * PI * .5) * .28;
      vec3 n = normalize(vec3(-g.x * dh, 1., -g.y * dh));
      vec3 L = normalize(vec3(-.45, .8, -.45));
      vec3 V = -ray;
      float dif = max(dot(n, L), 0.);
      float spec = pow(max(dot(reflect(-L, n), V), 0.), 36.);
      vec3 base = dark > .5 ? mix(vec3(.11, .12, .15), BRAND * .62, .6) : mix(vec3(1.), BRAND, .58);
      base = mix(base, dark > .5 ? BRAND * .95 : BRAND * .96, hi * .75);
      vec3 col = base * (.38 + .72 * dif) + vec3(1.) * spec * (dark > .5 ? .32 : .45);

      // The runner's trail along its lane.
      float rr = R0 + (k + .5) * LW;
      float du = fract(pick(uRun, k) - lapU(q, rr)) * perim(rr);
      float trail = exp(-du * (1.9 - .7 * hi)) * smoothstep(1., .15, abs(tt));
      col += BRAND * trail * (dark > .5 ? 1.15 : .7) + vec3(1., .85, .7) * trail * trail * .35;

      // Lane lines, one device pixel or more wide.
      float e = abs(fract(d / LW + .5) - .5) * LW;
      float line = 1. - smoothstep(.007 - aa, .007 + aa, e);
      col = mix(col, vec3(.96), line * (dark > .5 ? .8 : .95));
      // Curb on the outside.
      float curb = smoothstep(outer - aa, outer + aa, d);
      col = mix(col, dark > .5 ? vec3(.2, .21, .25) : vec3(.86, .87, .9), curb);

      float cover = (1. - smoothstep(outer + .05 - aa, outer + .05 + aa, d)) * smoothstep(-.02 - aa, -.02 + aa, d);
      float ta = cover * fog * copy * edge;
      ground = over(vec4(col * ta, ta), ground);
    }

    // A soft glow on the ground under each runner.
    for (int k = 0; k < 4; k++) {
      float fk = float(k);
      vec2 hk = lapPoint(pick(uRun, fk), R0 + (fk + .5) * LW);
      float gl = exp(-dot(q - hk, q - hk) / (.03 + .04 * pick(uLit, fk)));
      float ga = gl * (dark > .5 ? .55 : .35) * fog * copy * edge;
      ground = over(vec4(BRAND * ga, ga * .6), ground);
    }
  }

  // The runners: each lane's icon (Android, Apple, Web, API) cast in liquid
  // glass, with no tile behind it, projected so it faces the viewer. The
  // icon's own outline is the glass edge: clear, see-through, with a slow
  // holographic shimmer, a rim lit from the top left and a soft shadow. A
  // highlighted lane's icon warms to the brand orange.
  vec4 ball = vec4(0.);
  float rb = .17, bz = 1e9;
  for (int k = 0; k < 4; k++) {
    float fk = float(k);
    vec2 hk = lapPoint(pick(uRun, fk), R0 + (fk + .5) * LW);
    vec3 cw = vec3(hk.x, rb * .92, hk.y) - eye;
    float z = dot(cw, f);
    if (z < .1) continue;
    vec2 sc = vec2(dot(cw, rt), dot(cw, up)) / z * FOC * uCam.z;
    vec2 cs = vec2(uCam.x + sc.x, uCam.y - sc.y);
    float hi = pick(uLit, fk);
    float rs = rb * (1. + .28 * hi) / z * FOC * uCam.z;
    vec2 b = (p - cs) / rs;
    if (max(abs(b.x), abs(b.y)) < 1.15) {
      vec2 iuv = vec2((fk + .5 + b.x * .4) / 4., .5 + b.y * .4);
      float ic = texture2D(uIcons, iuv).a;

      // The glass edge: the icon's alpha gradient gives an outward normal
      // and how steep the edge is, which bends the surface into a bevel.
      vec2 ex = vec2(3. / 512., 0.), ey = vec2(0., 3. / 128.);
      vec2 gr = vec2(texture2D(uIcons, iuv + ex).a - texture2D(uIcons, iuv - ex).a,
                     texture2D(uIcons, iuv + ey).a - texture2D(uIcons, iuv - ey).a);
      float eg = clamp(length(gr), 0., 1.);
      vec2 out2 = eg > 1e-3 ? -gr / length(gr) : vec2(0.);
      float facing = dot(out2, normalize(vec2(-1., -1.)));   // light from the top left
      vec3 n = normalize(vec3(out2.x * eg * 1.8, -out2.y * eg * 1.8, 1.));
      vec3 L = normalize(vec3(-.55, .6, .58));
      float dif = max(dot(n, L), 0.);
      float sp = pow(max(dot(reflect(-L, n), vec3(0., 0., 1.)), 0.), 30.);

      // Holographic film: hue drifts across the icon and slowly in time.
      float hue = dot(b, vec2(.55, .8)) * .55 + uT * .025 + fk * .23;
      vec3 holo = .5 + .5 * cos(6.28318 * (hue + vec3(0., .33, .67)));
      vec3 film = mix(vec3(1.), holo, dark > .5 ? .38 : .34);
      film = mix(film, mix(vec3(1.), BRAND, .72), hi * .75);

      // Clear glass: the track shows through the middle, the bevelled edge is
      // dense and catches the light; the edge away
      // from the light sits in shade, which gives the glass its thickness.
      vec3 body = film * (.72 + .28 * dif);
      body *= 1. - eg * max(0., -facing) * (dark > .5 ? .4 : .32);
      float aBody = ic * ((dark > .5 ? .16 : .24) + (dark > .5 ? .72 : .86) * eg);   // clear inside, dense at the bevel
      // On a light page, glass shows as darker, refracted edges.
      if (dark < .5) body *= mix(1., .5, eg);
      vec4 glass = vec4(body * aBody, aBody);

      // Lit rim, specular glints and a diagonal sheen, all inside the shape.
      float sheen = (1. - smoothstep(0., .38, abs(b.x + b.y + .45))) * .3;
      float hl = clamp(eg * max(0., facing) * 1.2 + eg * .3 + sp * eg * .9 + sheen * .6, 0., 1.) * ic;
      glass = over(vec4(vec3(hl), hl * .9), glass);

      // A soft shadow below and to the right, outside the shape only.
      float ish = (texture2D(uIcons, iuv - vec2(.012 / 4., .035)).a + texture2D(uIcons, iuv - vec2(.024 / 4., .06)).a) * .5;
      float sa = ish * (1. - ic) * (dark > .5 ? .3 : .2);
      glass = over(glass, vec4(vec3(.12, .08, .1) * sa * .5, sa));

      float a = mix(1., copy, .6) * edge;
      vec4 g4 = glass * a;
      // Nearer badges cover farther ones, whatever the lane order.
      ball = z < bz ? over(g4, ball) : over(ball, g4);
      bz = min(bz, z);
    }
  }
  gl_FragColor = over(ball, ground);
}`;

// The four lane icons, white on transparent, 128 px each in one 512x128 strip,
// each within about 46 px of its centre (the shader shows up to 59): the
// Android robot, the Apple logo, a globe and a pair of braces. The shader
// turns each shape into glass, so they are solid or thick-stroked.
// (The Android robot is reproduced or modified from work created and shared
// by Google and used according to terms described in the Creative Commons
// 3.0 Attribution License. The Apple logo is a trademark of Apple Inc.; it
// is used here, at the owner's choice, without a licence from Apple.)
const APPLE_LOGO = 'M12.152 6.896c-.948 0-2.415-1.078-3.96-1.04-2.04.027-3.91 1.183-4.961 3.014-2.117 3.675-.546 9.103 1.519 12.09 1.013 1.454 2.208 3.09 3.792 3.039 1.52-.065 2.09-.987 3.935-.987 1.831 0 2.35.987 3.96.948 1.637-.026 2.676-1.48 3.676-2.948 1.156-1.688 1.636-3.325 1.662-3.415-.039-.013-3.182-1.221-3.22-4.857-.026-3.04 2.48-4.494 2.597-4.559-1.429-2.09-3.623-2.324-4.39-2.376-2-.156-3.675 1.09-4.61 1.09zM15.53 3.83c.843-1.012 1.4-2.427 1.245-3.83-1.207.052-2.662.805-3.532 1.818-.78.896-1.454 2.338-1.273 3.714 1.338.104 2.715-.688 3.559-1.701';
function drawIcons(): HTMLCanvasElement {
  const cv = document.createElement('canvas');
  cv.width = 512; cv.height = 128;
  const g = cv.getContext('2d')!;
  g.fillStyle = '#fff'; g.strokeStyle = '#fff'; g.lineCap = 'round'; g.lineJoin = 'round';

  // Android: the whole robot. Head with antennae and eyes, body, arms, legs.
  g.save(); g.translate(64, 64); g.scale(1.25, 1.25);
  g.beginPath(); g.moveTo(-19, -9); g.arc(0, -9, 19, Math.PI, 0); g.closePath(); g.fill();
  g.lineWidth = 3.5;
  g.beginPath(); g.moveTo(-9, -24); g.lineTo(-15, -35); g.moveTo(9, -24); g.lineTo(15, -35); g.stroke();
  g.beginPath(); g.roundRect(-19, -6, 38, 29, [0, 0, 6, 6]); g.fill();       // body
  g.beginPath(); g.roundRect(-30, -5, 8, 22, 4); g.roundRect(22, -5, 8, 22, 4); g.fill(); // arms
  g.beginPath(); g.roundRect(-12, 18, 8, 17, 4); g.roundRect(4, 18, 8, 17, 4); g.fill();  // legs
  g.globalCompositeOperation = 'destination-out';
  g.beginPath(); g.arc(-8, -16, 2.6, 0, Math.PI * 2); g.arc(8, -16, 2.6, 0, Math.PI * 2); g.fill();
  g.restore();

  // iOS: the Apple logo (a 24-unit path, scaled to about 88 px).
  g.save(); g.translate(192, 64); g.scale(3.65, 3.65); g.translate(-12, -12);
  g.fill(new Path2D(APPLE_LOGO));
  g.restore();

  // Web: a globe, thick enough to read as glass.
  g.save(); g.translate(320, 64);
  g.lineWidth = 7;
  g.beginPath(); g.arc(0, 0, 40, 0, Math.PI * 2); g.stroke();
  g.beginPath(); g.ellipse(0, 0, 16, 40, 0, 0, Math.PI * 2); g.stroke();
  g.beginPath(); g.moveTo(-40, 0); g.lineTo(40, 0); g.moveTo(-34, -20); g.lineTo(34, -20); g.moveTo(-34, 20); g.lineTo(34, 20); g.stroke();
  g.restore();

  // API: braces.
  g.save(); g.translate(448, 64); g.scale(1.1, 1.1);
  g.lineWidth = 7;
  const brace = (s: number) => {
    g.beginPath();
    g.moveTo(s * 10, -38); g.quadraticCurveTo(s * 22, -38, s * 22, -26);
    g.lineTo(s * 22, -10); g.quadraticCurveTo(s * 22, 0, s * 34, 0);
    g.quadraticCurveTo(s * 22, 0, s * 22, 10); g.lineTo(s * 22, 26);
    g.quadraticCurveTo(s * 22, 38, s * 10, 38);
    g.stroke();
  };
  g.save(); g.translate(4, 0); g.scale(-1, 1); brace(1); g.restore(); // {
  g.save(); g.translate(-4, 0); brace(1); g.restore();                 // }
  g.restore();
  return cv;
}

// The same geometry on the CPU, to find the lane under the pointer.
const A = 0.9, R0 = 1, LW = 0.17, FOC = 1.6, DIST = 5.4;
function laneAt(px: number, py: number, cam: number[], yaw: number, pitch: number): number {
  const eye = [DIST * Math.sin(yaw) * Math.cos(pitch), DIST * Math.sin(pitch), -DIST * Math.cos(yaw) * Math.cos(pitch)];
  const len = Math.hypot(eye[0], eye[1], eye[2]);
  const f = [-eye[0] / len, -eye[1] / len, -eye[2] / len];
  // rt = normalize(cross(up, f)), up' = cross(f, rt)
  let rt = [f[2], 0, -f[0]];
  const rl = Math.hypot(rt[0], rt[2]); rt = [rt[0] / rl, 0, rt[2] / rl];
  const up = [f[1] * rt[2] - f[2] * rt[1], f[2] * rt[0] - f[0] * rt[2], f[0] * rt[1] - f[1] * rt[0]];
  const ux = (px - cam[0]) / cam[2], uy = (cam[1] - py) / cam[2];
  const ray = [0, 1, 2].map((i) => f[i] * FOC + rt[i] * ux + up[i] * uy);
  if (ray[1] >= -1e-3) return -1;
  const t = -eye[1] / ray[1];
  const x = eye[0] + t * ray[0], z = eye[2] + t * ray[2];
  const d = Math.hypot(Math.max(Math.abs(x) - A, 0), z) - R0;
  return d >= 0 && d < 4 * LW ? Math.floor(d / LW) : -1;
}

export function startHeroTrack(stage: HTMLElement): void {
  const canvas = stage.querySelector<HTMLCanvasElement>('canvas.hero-track');
  if (!canvas) return;
  const gl = canvas.getContext('webgl', { alpha: true, premultipliedAlpha: true, antialias: false, powerPreference: 'low-power' });
  if (!gl || !gl.getExtension('OES_standard_derivatives')) return;

  const sh = (type: number, src: string) => {
    const s = gl.createShader(type)!;
    gl.shaderSource(s, src); gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) { console.warn(gl.getShaderInfoLog(s)); return null; }
    return s;
  };
  const vs = sh(gl.VERTEX_SHADER, VERT), fs = sh(gl.FRAGMENT_SHADER, FRAG);
  if (!vs || !fs) return;
  const prog = gl.createProgram()!;
  gl.attachShader(prog, vs); gl.attachShader(prog, fs);
  gl.bindAttribLocation(prog, 0, 'a');
  gl.linkProgram(prog);
  if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) return;
  gl.useProgram(prog);
  gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
  gl.enableVertexAttribArray(0);
  gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
  gl.clearColor(0, 0, 0, 0);
  // The lane icons, drawn once into a texture.
  const tex = gl.createTexture();
  gl.activeTexture(gl.TEXTURE0);
  gl.bindTexture(gl.TEXTURE_2D, tex);
  gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, drawIcons());
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  gl.uniform1i(gl.getUniformLocation(prog, 'uIcons'), 0);
  const U = (n: string) => gl.getUniformLocation(prog, n);
  const uRes = U('uRes'), uDpr = U('uDpr'), uT = U('uT'), uDark = U('uDark'), uCam = U('uCam'),
    uView = U('uView'), uMask = U('uMask'), uPtr = U('uPtr'), uRun = U('uRun'), uLit = U('uLit');

  stage.classList.add('gl');
  const reduce = matchMedia('(prefers-reduced-motion: reduce)');
  const chips = [...stage.querySelectorAll<HTMLButtonElement>('[data-lane]')];
  const readout = stage.querySelector<HTMLElement>('[data-readout]');
  const copyEl = stage.querySelector<HTMLElement>('[data-copy]');

  let W = 0, H = 0, dpr = 1;
  const cam = [0, 0, 300];
  const mask = [0, 0];
  const ptr = { x: 0, y: 0, on: 0, target: 0, nx: 0, ny: 0 };
  const view = { yaw: 0, pitch: 0.6 };
  // A staggered start, as in a real race: outer lanes start further round.
  const run = [0.0, 0.03, 0.06, 0.09];
  const speed = [0.56, 0.6, 0.53, 0.58];
  const lit = [0, 0, 0, 0], litTarget = [0, 0, 0, 0];
  let chipLane = -1, hoverLane = -1;

  const layout = () => {
    const r = stage.getBoundingClientRect();
    W = r.width; H = r.height;
    dpr = Math.min(devicePixelRatio || 1, 1.75);
    canvas.width = Math.max(1, Math.round(W * dpr));
    canvas.height = Math.max(1, Math.round(H * dpr));
    gl.viewport(0, 0, canvas.width, canvas.height);
    const wrapW = Math.min(1280, W) - 48, left = (W - wrapW) / 2;
    const c = copyEl?.getBoundingClientRect();
    if (W >= 1000) {
      cam[2] = Math.min(wrapW * 0.3, H * 0.66, 430);
      cam[0] = left + wrapW * 0.8;
      cam[1] = H * 0.5;
      mask[0] = c ? c.right - r.left : left + wrapW * 0.55; mask[1] = 0;
    } else {
      const bottomPad = parseFloat(getComputedStyle(stage).paddingBottom) || 240;
      cam[2] = Math.min(W * 0.5, 300);
      cam[0] = W / 2;
      cam[1] = H - bottomPad * 0.5;
      mask[0] = 0; mask[1] = c ? c.bottom - r.top : H - bottomPad;
    }
  };

  const setLane = (i: number) => {
    for (let k = 0; k < 4; k++) litTarget[k] = k === i ? 1 : 0;
    chips.forEach((b, k) => b.classList.toggle('is-on', k === i));
    if (readout) readout.textContent = i >= 0 ? chips[i]?.dataset.info ?? '' : readout.dataset.idle ?? '';
    kick();
  };
  const refreshLane = () => setLane(chipLane >= 0 ? chipLane : hoverLane);
  chips.forEach((b, k) => {
    const on = () => { chipLane = k; refreshLane(); };
    const off = () => { if (chipLane === k) { chipLane = -1; refreshLane(); } };
    b.addEventListener('pointerenter', on); b.addEventListener('pointerleave', off);
    b.addEventListener('focus', on); b.addEventListener('blur', off);
    b.addEventListener('click', on);
  });

  addEventListener('pointermove', (e) => {
    const r = canvas.getBoundingClientRect();
    const inside = e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
    const mouse = inside && e.pointerType === 'mouse';
    ptr.x = e.clientX - r.left; ptr.y = e.clientY - r.top;
    ptr.target = mouse ? 1 : 0;
    ptr.nx = mouse ? ptr.x / W - 0.5 : 0; ptr.ny = mouse ? ptr.y / H - 0.5 : 0;
    const lane = mouse ? laneAt(ptr.x, ptr.y, cam, view.yaw, view.pitch) : -1;
    if (lane !== hoverLane) { hoverLane = lane; refreshLane(); }
    kick();
  }, { passive: true });

  const isDark = () => (document.documentElement.dataset.theme === 'light' ? 0 : 1);
  let t0 = performance.now(), last = t0, raf = 0, visible = true, sway = { x: 0, y: 0 };

  const draw = (now: number) => {
    const t = reduce.matches ? 30 : (now - t0) / 1000;
    gl.uniform2f(uRes, W, H);
    gl.uniform1f(uDpr, dpr);
    gl.uniform1f(uT, t);
    gl.uniform1f(uDark, isDark());
    gl.uniform3f(uCam, cam[0], cam[1], cam[2]);
    gl.uniform2f(uView, view.yaw, view.pitch);
    gl.uniform2f(uMask, mask[0], mask[1]);
    gl.uniform3f(uPtr, ptr.x, ptr.y, ptr.on);
    gl.uniform4f(uRun, run[0], run[1], run[2], run[3]);
    gl.uniform4f(uLit, lit[0], lit[1], lit[2], lit[3]);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
  };

  const frame = (now: number) => {
    raf = 0;
    if (!visible || document.hidden) return;
    if (now - last < 30) { raf = requestAnimationFrame(frame); return; } // about 30 fps is plenty
    const dt = Math.min((now - last) / 1000, 0.1);
    last = now;
    let moving = false;
    for (let k = 0; k < 4; k++) {
      const dl = litTarget[k] - lit[k];
      if (Math.abs(dl) > 0.002) moving = true;
      lit[k] += dl * 0.2;
      if (!reduce.matches) {
        const r = R0 + (k + 0.5) * LW, P = 4 * A + 2 * Math.PI * r;
        run[k] = (run[k] + (dt * speed[k] * (1 + 0.9 * lit[k])) / P) % 1;
      }
    }
    const dp = ptr.target - ptr.on;
    if (Math.abs(dp) > 0.002) moving = true;
    ptr.on += dp * 0.15;
    const time = (now - t0) / 1000;
    sway.x += ((reduce.matches ? 0 : Math.sin(time * 0.11) * 0.1) + ptr.nx * 0.22 - sway.x) * 0.06;
    sway.y += (ptr.ny * 0.08 - sway.y) * 0.06;
    view.yaw = sway.x; view.pitch = 0.6 + sway.y;
    draw(now);
    if (reduce.matches && !moving) return;
    raf = requestAnimationFrame(frame);
  };
  const kick = () => { if (!raf) raf = requestAnimationFrame(frame); };

  new ResizeObserver(() => { layout(); draw(performance.now()); kick(); }).observe(stage);
  new IntersectionObserver(([e]) => { visible = e.isIntersecting; if (visible) { last = performance.now(); kick(); } }).observe(canvas);
  document.addEventListener('visibilitychange', () => { last = performance.now(); kick(); });
  new MutationObserver(() => { draw(performance.now()); kick(); }).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
  reduce.addEventListener?.('change', kick);
  layout();
  if (readout && !readout.dataset.idle) readout.dataset.idle = readout.textContent ?? '';
  draw(performance.now());
  kick();
}
