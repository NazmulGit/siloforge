import qrcode from 'qrcode-generator';
import { useMemo } from 'react';

/**
 * QR code drawn client-side as an SVG path (the secret never leaves the browser).
 * Always dark modules on white with a 4-module quiet zone, so phones scan it in dark mode too.
 */
export function QrCode({ text, size = 196, label = 'QR code' }: { text: string; size?: number; label?: string }) {
  const { d, n } = useMemo(() => {
    const qr = qrcode(0, 'M');
    qr.addData(text, 'Byte');
    qr.make();
    const count = qr.getModuleCount();
    let path = '';
    for (let r = 0; r < count; r++) {
      for (let c = 0; c < count; c++) {
        if (qr.isDark(r, c)) path += `M${c + 4} ${r + 4}h1v1h-1z`;
      }
    }
    return { d: path, n: count + 8 };
  }, [text]);

  return (
    <svg className="qr" width={size} height={size} viewBox={`0 0 ${n} ${n}`} role="img" aria-label={label} shapeRendering="crispEdges">
      <rect width={n} height={n} fill="#ffffff" />
      <path d={d} fill="#000000" />
    </svg>
  );
}
