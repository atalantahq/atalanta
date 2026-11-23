// The landing page's background: the data-stream tunnel from the first
// Atalanta site. A field of orange and cyan particles flows towards the
// viewer; scrolling makes it surge and the pointer tilts the camera.
// Fixed behind the page, procedural, no assets.
import {
  AdditiveBlending, BufferAttribute, BufferGeometry, Clock, Color, NormalBlending,
  PerspectiveCamera, Points, PointsMaterial, Scene, WebGLRenderer,
} from 'three';

export function startGalaxy(canvas: HTMLCanvasElement): void {
  let renderer: WebGLRenderer;
  try {
    renderer = new WebGLRenderer({ canvas, antialias: true, alpha: true, powerPreference: 'low-power' });
  } catch {
    canvas.remove();
    return;
  }
  renderer.setClearColor(0x000000, 0);
  renderer.setPixelRatio(Math.min(devicePixelRatio || 1, 1.5));

  const scene = new Scene();
  const camera = new PerspectiveCamera(60, 1, 0.1, 200);
  camera.position.z = 16;

  const COUNT = innerWidth < 700 ? 600 : 1400;
  const pos = new Float32Array(COUNT * 3);
  const col = new Float32Array(COUNT * 3);
  const vel = new Float32Array(COUNT);
  const orange = new Color(0xff6a3d), cyan = new Color(0x35e0ff), c = new Color();
  for (let i = 0; i < COUNT; i++) {
    const r = 7 + Math.random() * 28, a = Math.random() * Math.PI * 2;
    pos[i * 3] = Math.cos(a) * r;
    pos[i * 3 + 1] = Math.sin(a) * r;
    pos[i * 3 + 2] = -120 + Math.random() * 150;
    vel[i] = 0.05 + Math.random() * 0.2;
    c.copy(orange).lerp(cyan, Math.pow(Math.random(), 1.8)); // mostly orange, some cyan
    col[i * 3] = c.r; col[i * 3 + 1] = c.g; col[i * 3 + 2] = c.b;
  }
  const geo = new BufferGeometry();
  geo.setAttribute('position', new BufferAttribute(pos, 3));
  geo.setAttribute('color', new BufferAttribute(col, 3));
  const mat = new PointsMaterial({ size: 0.13, vertexColors: true, transparent: true, depthWrite: false });
  const particles = new Points(geo, mat);
  scene.add(particles);

  // Additive light reads as glow on a dark page and vanishes on a white one,
  // so the light theme draws the particles as plain, softer dots.
  const applyTheme = () => {
    const light = document.documentElement.dataset.theme === 'light';
    mat.blending = light ? NormalBlending : AdditiveBlending;
    mat.opacity = light ? 0.42 : 0.7;
    mat.size = light ? 0.11 : 0.13;
    mat.needsUpdate = true;
  };
  applyTheme();
  new MutationObserver(applyTheme).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });

  const reduce = matchMedia('(prefers-reduced-motion: reduce)').matches;
  let tx = 0, ty = 0, cx = 0, cy = 0, scrollN = 0, vel0 = 0, lastY = scrollY;
  addEventListener('pointermove', (e) => { tx = e.clientX / innerWidth - 0.5; ty = e.clientY / innerHeight - 0.5; }, { passive: true });
  addEventListener('scroll', () => {
    const dy = Math.abs(scrollY - lastY); lastY = scrollY;
    vel0 = Math.min(Math.max(vel0, dy * 0.02), 3);
    const max = document.documentElement.scrollHeight - innerHeight;
    scrollN = max > 0 ? scrollY / max : 0;
    if (reduce) render(0);
  }, { passive: true });
  const resize = () => {
    renderer.setSize(innerWidth, innerHeight, false);
    camera.aspect = innerWidth / innerHeight;
    camera.updateProjectionMatrix();
    if (reduce) render(0);
  };
  addEventListener('resize', resize);
  resize();

  const clock = new Clock();
  const render = (dt: number) => {
    const t = clock.elapsedTime;
    const boost = 1 + vel0 * 6;
    for (let i = 0; i < COUNT; i++) {
      pos[i * 3 + 2] += vel[i] * boost * dt * 60 * 0.6 + dt * 2;
      if (pos[i * 3 + 2] > camera.position.z + 4) pos[i * 3 + 2] = -120;
    }
    geo.attributes.position.needsUpdate = true;
    vel0 *= 0.92;
    cx += (tx - cx) * 0.05; cy += (ty - cy) * 0.05;
    particles.rotation.z = t * 0.01;
    camera.position.x += (cx * 3 - camera.position.x) * 0.05;
    camera.position.y += (-cy * 3 - camera.position.y) * 0.05 + (scrollN * 2 - camera.position.y) * 0.02;
    camera.position.z = 16 - scrollN * 5;
    camera.lookAt(0, 0, camera.position.z - 20);
    renderer.render(scene, camera);
  };

  if (reduce) { render(0); return; }
  let raf = 0;
  const loop = () => {
    raf = 0;
    if (document.hidden) return;
    render(Math.min(clock.getDelta(), 0.05));
    raf = requestAnimationFrame(loop);
  };
  document.addEventListener('visibilitychange', () => { if (!document.hidden && !raf) { clock.getDelta(); loop(); } });
  loop();
}
