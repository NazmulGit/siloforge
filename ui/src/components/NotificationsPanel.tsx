import { useCallback, useEffect, useState } from 'react';
import {
  createChannel,
  deleteChannel,
  fetchChannels,
  isUnavailable,
  patchChannel,
  testChannel,
  v2ErrorMessage,
} from '../api2';
import { useToast } from '../toast';
import {
  CHANNEL_KINDS,
  NOTIFY_EVENTS,
  type ChannelInput,
  type ChannelKind,
  type NotificationChannel,
} from '../types2';
import { Modal } from './Modal';
import { Skeleton } from './Skeleton';
import { Toggle } from './Toggle';
import { FeatureUnavailable } from './V2Notice';

interface FieldDef {
  key: string;
  label: string;
  placeholder?: string;
  /** Write-only: never shown back, blank on edit means "keep". */
  secret?: boolean;
  required?: boolean;
  /** Value is a JSON object typed as text. */
  json?: boolean;
}

const KIND_FIELDS: Record<ChannelKind, FieldDef[]> = {
  webhook: [
    { key: 'url', label: 'Webhook URL', placeholder: 'https://example.com/hooks/siloforge', required: true },
    { key: 'headers', label: 'Extra headers (JSON object, optional)', placeholder: '{"Authorization": "Bearer …"}', secret: true, json: true },
  ],
  ntfy: [
    { key: 'url', label: 'Topic URL', placeholder: 'https://ntfy.sh/siloforge-alerts', required: true },
    { key: 'token', label: 'Access token (optional)', secret: true },
  ],
  telegram: [
    { key: 'bot_token', label: 'Bot token', secret: true, required: true },
    { key: 'chat_id', label: 'Chat ID', placeholder: '-1001234567890', required: true },
    { key: 'thread_id', label: 'Topic / thread ID (optional)' },
  ],
  hermes: [
    { key: 'url', label: 'Hermes webhook URL', placeholder: 'http://127.0.0.1:<port>/webhooks/siloforge', required: true },
    { key: 'secret', label: 'HMAC secret (optional)', secret: true },
  ],
  desktop: [],
  log: [],
};

const KIND_HINTS: Record<ChannelKind, string> = {
  webhook: 'POSTs a JSON event to any URL, with optional extra headers.',
  ntfy: 'Push to phones/desktops through an ntfy topic URL.',
  telegram: 'Sends a message from your bot to a chat or group.',
  hermes: 'Notifies a Hermes Agent webhook (HMAC-signed if a secret is set) so it can react to jobs.',
  desktop: 'Windows toast on the machine running SiloForge.',
  log: 'Writes the event to the server log only (useful for testing).',
};

const EVENT_LABELS: Record<string, string> = {
  'job.failed': 'Job failed',
  'job.done': 'Job finished',
  'backup.done': 'Backup finished',
  'backup.failed': 'Backup failed',
  'system.alert': 'System alert',
  'plan.ready': 'Plan ready',
  'inject.done': 'Links injected',
};

interface FormState {
  id: string | null;
  name: string;
  kind: ChannelKind;
  config: Record<string, string>;
  events: string[];
  enabled: boolean;
}

function emptyForm(): FormState {
  return { id: null, name: '', kind: 'webhook', config: {}, events: ['job.failed', 'backup.failed', 'system.alert'], enabled: true };
}

function formFrom(ch: NotificationChannel): FormState {
  const config: Record<string, string> = {};
  for (const f of KIND_FIELDS[ch.kind] ?? []) {
    if (f.secret) continue; // write-only
    const v = ch.config?.[f.key];
    if (typeof v === 'string' || typeof v === 'number') config[f.key] = String(v);
  }
  return { id: ch.id, name: ch.name, kind: ch.kind, config, events: [...ch.events], enabled: ch.enabled };
}

function configSummary(ch: NotificationChannel): string {
  const fields = KIND_FIELDS[ch.kind] ?? [];
  const parts: string[] = [];
  for (const f of fields) {
    if (f.secret) continue;
    const v = ch.config?.[f.key];
    if (typeof v === 'string' && v) parts.push(v);
  }
  return parts.join(' · ') || '—';
}

function ChannelForm({ initial, onClose, onSaved }: { initial: FormState; onClose: () => void; onSaved: () => void }) {
  const toast = useToast();
  const [form, setForm] = useState<FormState>(initial);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const editing = form.id !== null;
  const fields = KIND_FIELDS[form.kind];

  async function save() {
    setError(null);
    if (!form.name.trim()) {
      setError('Give the channel a name.');
      return;
    }
    for (const f of fields) {
      const v = (form.config[f.key] ?? '').trim();
      if (f.required && !v && !(editing && f.secret)) {
        setError(`${f.label} is required.`);
        return;
      }
    }
    // Only send secrets the user actually typed: blank = keep the stored value.
    const config: Record<string, unknown> = {};
    for (const f of fields) {
      const v = (form.config[f.key] ?? '').trim();
      if (!v && (f.secret || !f.required)) continue;
      if (f.json) {
        try {
          const parsed: unknown = JSON.parse(v);
          if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) throw new Error('not an object');
          config[f.key] = parsed;
        } catch {
          setError(`${f.label}: enter a JSON object.`);
          return;
        }
        continue;
      }
      config[f.key] = v;
    }
    const body: ChannelInput = { name: form.name.trim(), kind: form.kind, config, events: form.events, enabled: form.enabled };
    setBusy(true);
    try {
      if (form.id) {
        const { kind: _kind, ...patch } = body;
        void _kind;
        await patchChannel(form.id, patch);
      } else await createChannel(body);
      toast.push(editing ? 'Channel saved' : 'Channel added', 'success');
      onSaved();
    } catch (e) {
      setError(v2ErrorMessage(e, 'Could not save channel'));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      title={editing ? 'Edit channel' : 'Add notification channel'}
      onClose={onClose}
      footer={
        <>
          <button className="btn" onClick={onClose}>
            Cancel
          </button>
          <button className="btn btn-primary" disabled={busy} onClick={() => void save()}>
            {busy ? 'Saving…' : 'Save'}
          </button>
        </>
      }
    >
      <div className="form-grid" style={{ maxWidth: 'none' }}>
        {error && <div className="error-box">{error}</div>}
        <label className="field">
          Name
          <input type="text" value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} placeholder="Ops phone" />
        </label>
        <label className="field">
          Kind
          <select
            value={form.kind}
            disabled={editing}
            onChange={(e) => setForm({ ...form, kind: e.target.value as ChannelKind, config: {} })}
          >
            {CHANNEL_KINDS.map((k) => (
              <option key={k} value={k}>
                {k}
              </option>
            ))}
          </select>
          <span className="faint" style={{ fontSize: 12 }}>
            {KIND_HINTS[form.kind]}
          </span>
        </label>
        {fields.map((f) => (
          <label className="field" key={f.key}>
            {f.label}
            <input
              type={f.secret ? 'password' : 'text'}
              autoComplete="off"
              value={form.config[f.key] ?? ''}
              placeholder={f.secret && editing ? '(unchanged — type to replace)' : f.placeholder}
              onChange={(e) => setForm({ ...form, config: { ...form.config, [f.key]: e.target.value } })}
            />
          </label>
        ))}
        <div className="field">
          Events
          <div style={{ display: 'flex', flexWrap: 'wrap', gap: '6px 14px', marginTop: 4 }}>
            {NOTIFY_EVENTS.map((ev) => (
              <label key={ev} style={{ display: 'inline-flex', gap: 6, alignItems: 'center', fontSize: 13 }}>
                <input
                  type="checkbox"
                  checked={form.events.includes(ev)}
                  onChange={(e) =>
                    setForm({
                      ...form,
                      events: e.target.checked ? [...form.events, ev] : form.events.filter((x) => x !== ev),
                    })
                  }
                />
                {EVENT_LABELS[ev] ?? ev}
              </label>
            ))}
          </div>
        </div>
        <label style={{ display: 'inline-flex', gap: 8, alignItems: 'center', fontSize: 13 }}>
          <input type="checkbox" checked={form.enabled} onChange={(e) => setForm({ ...form, enabled: e.target.checked })} /> Enabled
        </label>
      </div>
    </Modal>
  );
}

/** Settings → Notifications: CRUD for delivery channels + "Send test". */
export function NotificationsPanel() {
  const toast = useToast();
  const [channels, setChannels] = useState<NotificationChannel[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [editing, setEditing] = useState<FormState | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setChannels(await fetchChannels());
      setError(null);
    } catch (e) {
      if (isUnavailable(e)) setUnavailable(true);
      else setError(v2ErrorMessage(e, 'Failed to load channels'));
      setChannels([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function toggle(ch: NotificationChannel, enabled: boolean) {
    setBusy(ch.id);
    try {
      await patchChannel(ch.id, { enabled });
      await load();
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Could not update channel'), 'error');
    } finally {
      setBusy(null);
    }
  }

  async function test(ch: NotificationChannel) {
    setBusy(ch.id);
    try {
      const r = await testChannel(ch.id);
      if (r.ok) toast.push(`Test sent to ${ch.name}`, 'success');
      else toast.push(`Test failed: ${r.error ?? 'unknown error'}`, 'error');
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Test failed'), 'error');
    } finally {
      setBusy(null);
    }
  }

  async function remove(ch: NotificationChannel) {
    if (!window.confirm(`Delete channel "${ch.name}"?`)) return;
    setBusy(ch.id);
    try {
      await deleteChannel(ch.id);
      toast.push('Channel deleted', 'success');
      await load();
    } catch (e) {
      toast.push(v2ErrorMessage(e, 'Could not delete channel'), 'error');
    } finally {
      setBusy(null);
    }
  }

  if (unavailable) {
    return (
      <div className="card">
        <FeatureUnavailable feature="Notification channels" />
      </div>
    );
  }

  return (
    <div className="card">
      <div className="table-toolbar" style={{ marginBottom: 12 }}>
        <div className="card-title" style={{ marginBottom: 0 }}>
          Channels
        </div>
        <div className="spacer" />
        <button className="btn btn-primary btn-sm" onClick={() => setEditing(emptyForm())}>
          ＋ Add channel
        </button>
      </div>
      {error && <div className="error-box" style={{ marginBottom: 10 }}>{error}</div>}
      {channels === null ? (
        <Skeleton rows={3} />
      ) : channels.length === 0 ? (
        <div className="muted">
          No channels yet. Notifications still appear under the bell in the top bar; add a channel to also get them by
          webhook, ntfy, Telegram, Hermes or desktop toast.
        </div>
      ) : (
        <div className="tbl-wrap">
          <table className="tbl">
            <thead>
              <tr>
                <th>On</th>
                <th>Name</th>
                <th>Kind</th>
                <th>Target</th>
                <th>Events</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {channels.map((ch) => (
                <tr key={ch.id}>
                  <td>
                    <Toggle checked={ch.enabled} disabled={busy === ch.id} onChange={(v) => void toggle(ch, v)} label={`Enable ${ch.name}`} />
                  </td>
                  <td>{ch.name}</td>
                  <td className="mono">{ch.kind}</td>
                  <td className="muted" style={{ maxWidth: 260, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    {configSummary(ch)}
                  </td>
                  <td className="muted" style={{ fontSize: 12.5 }}>
                    {ch.events.map((e) => EVENT_LABELS[e] ?? e).join(', ') || '—'}
                  </td>
                  <td>
                    <div className="row-actions">
                      <button className="btn btn-sm" disabled={busy === ch.id} onClick={() => void test(ch)} data-hint="Send a test notification through this channel now.">
                        Send test
                      </button>
                      <button className="btn btn-sm" onClick={() => setEditing(formFrom(ch))}>
                        Edit
                      </button>
                      <button className="btn btn-sm btn-danger" disabled={busy === ch.id} onClick={() => void remove(ch)}>
                        Delete
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {editing && (
        <ChannelForm
          initial={editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            void load();
          }}
        />
      )}
    </div>
  );
}
