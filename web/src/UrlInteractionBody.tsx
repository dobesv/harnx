import type { PendingInteraction } from './harnxCustomEvents';

/**
 * Body of a `url` interaction (decision 6): the link opens in a new tab with
 * `noopener noreferrer`, and the user answers with Continue/Cancel. The
 * buttons live in `InteractionUI`'s action row next to this body; the
 * response carries the action alone, never data.
 */
export const UrlInteractionBody = ({ interaction }: { interaction: PendingInteraction }) => {
  const url = interaction.url ?? '';
  if (url.length === 0) {
    return (
      <div
        className="aui-interrupt-unsupported"
        role="alert"
        style={{ color: '#c62828', marginTop: '8px', marginBottom: '8px', fontWeight: 500 }}
      >
        Unable to render link
      </div>
    );
  }
  return (
    <p className="aui-interrupt-message" data-testid="url-interaction-link-row">
      <a
        href={url}
        target="_blank"
        rel="noopener noreferrer"
        data-testid="url-interaction-link"
      >
        {url}
      </a>
    </p>
  );
};
