import { useId } from "react";

/**
 * 四个应用的手绘风格大图标（原创插画，不是各家的官方 logo）。
 * - 描边用 `currentColor`（浅色深色主题下由外层决定墨水色），颜色块是错位印刷的马克笔效果；
 * - 整体套一层很轻的位移滤镜让线条有手抖的感觉；每个实例的滤镜 id 唯一，同一页放多个不会串。
 * - 纯装饰：本身 aria-hidden，名字和状态由外面的文字承担。
 */

export interface AppIconProps {
  /** claude_cli / codex_exec / agy / deepseek */
  source: string;
  /** 边长（像素），默认 120 */
  size?: number;
  className?: string;
}

const INK = 4.5;
/** 画在奶油色、白色色块上的细节用固定的深墨色：外轮廓跟随主题（currentColor），里面的细节不能跟着变浅，否则深色主题下看不见 */
const DARK_INK = "#2b2622";

/** Claude：陶土色的手绘放射星，中间一团奶油色。 */
function ClaudeArt() {
  const rays = [
    [0, 58], [40, 40], [80, 60], [120, 36], [160, 56], [200, 42], [240, 60], [280, 38], [320, 54],
  ];
  return (
    <g>
      <g transform="translate(4 4)" stroke="#e9a68d" strokeWidth={13} strokeLinecap="round" fill="none" opacity={0.85}>
        {rays.map(([deg, len]) => {
          const a = ((deg - 90) * Math.PI) / 180;
          return <line key={deg} x1={80 + Math.cos(a) * 16} y1={84 + Math.sin(a) * 16} x2={80 + Math.cos(a) * len} y2={84 + Math.sin(a) * len} />;
        })}
      </g>
      <g stroke="#cc785c" strokeWidth={9} strokeLinecap="round" fill="none">
        {rays.map(([deg, len]) => {
          const a = ((deg - 90) * Math.PI) / 180;
          return <line key={deg} x1={80 + Math.cos(a) * 15} y1={80 + Math.sin(a) * 15} x2={80 + Math.cos(a) * (len - 4)} y2={80 + Math.sin(a) * (len - 4)} />;
        })}
      </g>
      <path d="M80 64 C92 63 99 71 98 82 C97 93 88 98 78 97 C67 96 62 88 63 78 C64 69 71 65 80 64 Z" fill="#fff4e6" stroke="currentColor" strokeWidth={INK} strokeLinejoin="round" />
      <path d="M72 80 C75 85 85 85 88 80" fill="none" stroke={DARK_INK} strokeWidth={3.5} strokeLinecap="round" />
    </g>
  );
}

/** Codex：一个手绘的终端窗口，里面是 `> _`。 */
function CodexArt() {
  const body = "M22 40 C22 31 28 27 37 27 L124 25 C133 25 139 31 139 40 L138 116 C138 125 132 131 123 131 L37 132 C28 132 22 126 22 117 Z";
  return (
    <g>
      <path d={body} transform="translate(5 5)" fill="#7fe0c3" />
      <path d={body} fill="#fffdf6" stroke="currentColor" strokeWidth={INK} strokeLinejoin="round" />
      <path d="M23 54 C60 52 100 52 138 51" fill="none" stroke={DARK_INK} strokeWidth={3.5} strokeLinecap="round" />
      <circle cx={35} cy={40} r={4.5} fill="#ff8f7a" stroke={DARK_INK} strokeWidth={2.5} />
      <circle cx={49} cy={40} r={4.5} fill="#ffd166" stroke={DARK_INK} strokeWidth={2.5} />
      <circle cx={63} cy={40} r={4.5} fill="#7fe0c3" stroke={DARK_INK} strokeWidth={2.5} />
      <path d="M44 78 C52 83 58 88 65 92 C58 97 51 102 44 107" fill="none" stroke="#0f9d7a" strokeWidth={8} strokeLinecap="round" strokeLinejoin="round" />
      <path d="M44 78 C52 83 58 88 65 92 C58 97 51 102 44 107" fill="none" stroke={DARK_INK} strokeWidth={3} strokeLinecap="round" strokeLinejoin="round" />
      <path d="M78 109 C88 108 98 109 108 108" fill="none" stroke={DARK_INK} strokeWidth={6} strokeLinecap="round" />
    </g>
  );
}

/** Gemini（agy）：一大一小两颗手绘的四角星，蓝紫渐变。 */
function GeminiArt({ gradient }: { gradient: string }) {
  const big = "M80 16 C84 54 108 77 146 81 C108 85 84 109 80 146 C76 109 52 85 14 81 C52 77 76 54 80 16 Z";
  const small = "M128 20 C129 31 135 37 146 38 C135 39 129 45 128 56 C127 45 121 39 110 38 C121 37 127 31 128 20 Z";
  return (
    <g>
      <path d={big} transform="translate(5 5)" fill="#c9d4ff" />
      <path d={big} fill={`url(#${gradient})`} stroke="currentColor" strokeWidth={INK} strokeLinejoin="round" />
      <path d={small} transform="translate(3 3)" fill="#ffd9f0" />
      <path d={small} fill="#ffb3e0" stroke="currentColor" strokeWidth={4} strokeLinejoin="round" />
      <path d="M62 78 C66 70 72 66 80 64" fill="none" stroke="#fff" strokeWidth={4} strokeLinecap="round" opacity={0.85} />
    </g>
  );
}

/** DeepSeek：一头手绘的小鲸鱼，头顶喷着水。 */
function DeepSeekArt() {
  const body = "M30 98 C28 64 66 46 99 54 C122 59 140 74 141 94 C142 112 127 124 107 126 C85 129 65 128 47 120 C36 115 31 107 30 98 Z";
  const tail = "M34 96 C20 94 11 84 11 71 C20 76 28 76 35 73 C29 63 29 53 35 44 C41 57 47 63 55 68";
  return (
    <g>
      <path d={body} transform="translate(5 5)" fill="#a9baff" />
      <path d={body} fill="#4d6bfe" stroke="currentColor" strokeWidth={INK} strokeLinejoin="round" />
      <path d={tail} fill="#4d6bfe" stroke="currentColor" strokeWidth={INK} strokeLinecap="round" strokeLinejoin="round" />
      <path d="M52 113 C78 122 104 120 125 109" fill="none" stroke="#dfe6ff" strokeWidth={9} strokeLinecap="round" />
      <path d="M97 50 C95 39 89 32 80 29 M101 50 C103 38 109 31 119 28 M99 49 L99 31" fill="none" stroke="#7fb8ff" strokeWidth={5} strokeLinecap="round" />
      <circle cx={117} cy={85} r={5.5} fill="#fff" stroke={DARK_INK} strokeWidth={3} />
      <circle cx={118.5} cy={86} r={2.2} fill={DARK_INK} />
      <path d="M126 98 C130 101 135 101 138 98" fill="none" stroke="currentColor" strokeWidth={3} strokeLinecap="round" />
    </g>
  );
}

export default function AppIcon({ source, size = 120, className }: AppIconProps) {
  const uid = useId().replace(/:/g, "");
  const wobble = `wobble-${uid}`;
  const gradient = `grad-${uid}`;
  return (
    <svg className={className} width={size} height={size} viewBox="0 0 160 160" aria-hidden="true" focusable="false">
      <defs>
        {/* 手抖效果：低频噪声推动线条，幅度很小，只让边缘不那么「机器」 */}
        <filter id={wobble} x="-5%" y="-5%" width="110%" height="110%">
          <feTurbulence type="fractalNoise" baseFrequency="0.035" numOctaves="2" seed="7" result="noise" />
          <feDisplacementMap in="SourceGraphic" in2="noise" scale="3.2" xChannelSelector="R" yChannelSelector="G" />
        </filter>
        <linearGradient id={gradient} x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#5b9dff" />
          <stop offset="1" stopColor="#b57bff" />
        </linearGradient>
      </defs>
      <g filter={`url(#${wobble})`}>
        {source === "claude_cli" && <ClaudeArt />}
        {source === "codex_exec" && <CodexArt />}
        {source === "agy" && <GeminiArt gradient={gradient} />}
        {source === "deepseek" && <DeepSeekArt />}
      </g>
    </svg>
  );
}
