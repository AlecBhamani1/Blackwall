# Blackwall desktop bridge

The Tauri shell does not launch or manage a model process. It connects to an
OpenAI-compatible endpoint that is already running, including Ollama on another
machine. Endpoint selection uses this precedence:

1. `ChatRequest.endpoint`, when explicitly provided by a trusted client;
2. `BLACKWALL_MODEL_ENDPOINT`;
3. `OLLAMA_HOST`; and
4. `http://localhost:11434/v1`, the implementation-plan fallback.

An origin without a path receives `/v1` automatically, so both
`http://model-host:11434` and `http://model-host:11434/v1` work. Host-and-port
values without a scheme are treated as HTTP. For example:

```sh
BLACKWALL_MODEL_ENDPOINT=http://192.168.1.50:11434/v1 \
  ../../ui/node_modules/.bin/tauri dev
```

The optional `BLACKWALL_MODEL_API_KEY` is read into process memory and sent as a
Bearer token. Blackwall does not write the endpoint or key to source, local
storage, or configuration in this scaffold. Keychain-backed settings replace
this environment-only bridge in the security milestone.

Only expose an Ollama listener on a network you trust. The later Blackwall share
gateway will authenticate guests instead of exposing the raw model endpoint.
