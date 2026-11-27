import { getLocale } from "./locale";

export function WaterLevel({ value, total, label, moving = false }: {
  value: number; total: number; label: string; moving?: boolean;
}) {
  const level = total > 0 ? Math.min(1, Math.max(0, value / total)) : 0;
  const percent = new Intl.NumberFormat(getLocale(), { style: "percent", maximumFractionDigits: 1 }).format(level);
  return <div className="water-level" role="progressbar" aria-label={label} aria-valuemin={0} aria-valuemax={100}
    aria-valuenow={Math.round(level * 1000) / 10} aria-valuetext={percent}
    data-moving={moving && level < 1} data-empty={level === 0}>
    <span className="water-fill" aria-hidden="true" style={{ transform: `translateY(${(1 - level) * 100}%)` }}>
      <span className="water-surface" />
    </span>
    <strong aria-hidden="true">{percent}</strong>
  </div>;
}
