interface ToggleProps {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: string;
  dataHint?: string;
  disabled?: boolean;
}

/** Accessible switch: role="switch", operable with Space / Enter. */
export function Toggle({ checked, onChange, label, dataHint, disabled }: ToggleProps) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      className={`toggle${checked ? ' on' : ''}`}
      data-hint={dataHint}
      disabled={disabled}
      onClick={() => onChange(!checked)}
    >
      <span className="toggle-knob" />
    </button>
  );
}
