/** Shown in place of a v2 panel when the server doesn't offer the feature (404/503). */
export function FeatureUnavailable({ feature }: { feature: string }) {
  return (
    <div className="empty">
      <div className="empty-title">Feature not available on this server</div>
      <div className="empty-hint">
        {feature} needs SiloForge 2.0 or later. This server did not answer the v2 API (it may be an older
        version, or the feature is switched off).
      </div>
    </div>
  );
}
