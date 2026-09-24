/** Native command rejections are serialized objects, not JavaScript Errors. */
export class ModelRequestError extends Error {
  constructor(
    message: string,
    readonly code: string,
  ) {
    super(message);
    this.name = 'ModelRequestError';
  }

  get connectionLost(): boolean {
    return [
      'model_unavailable',
      'model_access_denied',
      'model_timeout',
      'model_not_found',
      'credential_error',
    ].includes(this.code);
  }
}

export function nativeModelError(cause: unknown): ModelRequestError {
  const record = cause && typeof cause === 'object' ? (cause as Record<string, unknown>) : {};
  const code = typeof record.code === 'string' ? record.code : '';
  // Allowlisted copy also protects against raw URLs/bodies from older native builds.
  const messages: Record<string, string> = {
    storage_error:
      'Blackwall could not access its local data. Check disk space and folder permissions, then retry saving. Keep Blackwall open to preserve unsaved work.',
    credential_error:
      'Blackwall could not access the saved key. Respond to any macOS Keychain prompt, unlock your login keychain if needed, then reconnect.',
    model_unavailable:
      'The model computer is unavailable. Open and unlock Blackwall on that computer, keep its model running, and reconnect.',
    model_access_denied:
      'Model access was refused or removed. Check your access key, or pair the computer again if its access was removed.',
    model_timeout:
      'The model took too long to respond. Check that its computer is awake and connected, then try again.',
    model_busy: 'The model is busy. Wait for another request to finish, then try again.',
    model_not_found:
      'The model connection is no longer available. Check that its computer is open, unlocked, and awake. If access was removed, pair again.',
    invalid_request:
      'The request could not be sent. Check the connection settings, model, and attachments.',
    model_response_too_large: 'The model response was too large. Ask for a shorter response.',
    empty_model_response:
      'The model returned no answer. Try a shorter prompt or choose another model.',
    invalid_model_response:
      'The model returned an unreadable response. Try again or check the model service.',
  };
  return new ModelRequestError(
    Object.hasOwn(messages, code)
      ? messages[code]
      : 'The model could not complete the request. Try again or check the model service.',
    code,
  );
}
