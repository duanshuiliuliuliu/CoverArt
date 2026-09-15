/* 生成应用图标源图（1024×1024 PNG，纯 Node，无依赖）：
   深色圆角方块 + 一枚白色唱片环 + 中心点，和界面里那枚「详情」图标同一个母题。
   用法：node scripts/gen_icon.mjs && npm run icon */
import fs from "node:fs";
import path from "node:path";
import zlib from "node:zlib";

const S = 1024;
const OUT = path.join(import.meta.dirname, "icon.png");

const px = Buffer.alloc(S * S * 4);
const clampi = (v) => (v < 0 ? 0 : v > 255 ? 255 : v | 0);
const smooth = (edge0, edge1, x) => {
  const t = Math.min(1, Math.max(0, (x - edge0) / (edge1 - edge0)));
  return t * t * (3 - 2 * t);
};

/* 圆角矩形的有符号距离场 */
function roundRectSDF(x, y, cx, cy, hw, hh, r){
  const dx = Math.abs(x - cx) - (hw - r);
  const dy = Math.abs(y - cy) - (hh - r);
  const ox = Math.max(dx, 0), oy = Math.max(dy, 0);
  return Math.hypot(ox, oy) + Math.min(Math.max(dx, dy), 0) - r;
}

for (let y = 0; y < S; y++){
  for (let x = 0; x < S; x++){
    const cx = x + 0.5, cy = y + 0.5;
    const i = (y * S + x) * 4;

    /* 底：深色圆角方块，带一点竖直渐变 */
    const dRect = roundRectSDF(cx, cy, S / 2, S / 2, S / 2 - 26, S / 2 - 26, 210);
    const aRect = 1 - smooth(-1.2, 1.2, dRect);
    const t = cy / S;
    let r = 26 + 14 * (1 - t), g = 26 + 14 * (1 - t), b = 30 + 16 * (1 - t);

    /* 唱片环 */
    const dRing = Math.abs(Math.hypot(cx - S / 2, cy - S / 2) - 300) - 26;
    const aRing = 1 - smooth(-1.2, 1.2, dRing);
    /* 中心点 */
    const dDot = Math.hypot(cx - S / 2, cy - S / 2) - 62;
    const aDot = 1 - smooth(-1.2, 1.2, dDot);
    const ink = Math.max(aRing, aDot);
    r = r * (1 - ink) + 246 * ink;
    g = g * (1 - ink) + 246 * ink;
    b = b * (1 - ink) + 248 * ink;

    px[i] = clampi(r);
    px[i + 1] = clampi(g);
    px[i + 2] = clampi(b);
    px[i + 3] = clampi(aRect * 255);
  }
}

/* 手工组 PNG：IHDR + IDAT + IEND */
function chunk(type, data){
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body) >>> 0);
  return Buffer.concat([len, body, crc]);
}
const CRC_TABLE = (() => {
  const t = new Int32Array(256);
  for (let n = 0; n < 256; n++){
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c;
  }
  return t;
})();
function crc32(buf){
  let c = -1;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
}

const raw = Buffer.alloc((S * 4 + 1) * S);
for (let y = 0; y < S; y++){
  raw[y * (S * 4 + 1)] = 0;                                  /* filter: none */
  px.copy(raw, y * (S * 4 + 1) + 1, y * S * 4, (y + 1) * S * 4);
}

const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(S, 0);
ihdr.writeUInt32BE(S, 4);
ihdr[8] = 8;      /* bit depth */
ihdr[9] = 6;      /* RGBA */
const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk("IHDR", ihdr),
  chunk("IDAT", zlib.deflateSync(raw, { level: 9 })),
  chunk("IEND", Buffer.alloc(0)),
]);

fs.writeFileSync(OUT, png);
console.log("icon written:", OUT, png.length, "bytes");
