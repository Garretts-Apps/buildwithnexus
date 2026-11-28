# Providers

## Gemini, xAI, Mistral, DeepSeek

Gemini, xAI, Mistral and DeepSeek use their own OpenAI-compatible hosts
(DeepSeek's has no `/v1`; Google's compatibility endpoint is still a beta).
A bare `gemini…` name selects the Gemini preset. `"route_gemini":
"openrouter"` or `--route-gemini openrouter` restores the old OpenRouter
`google/<name>` route. With no settings file, a headless run still prefers
an Anthropic, OpenAI, OpenRouter, Groq, or Hugging Face key when one is set.

## Cloud providers

bwn reaches models in your own cloud account with credentials that stay
bound to that cloud's endpoints (roadmap 4.4). A cloud credential is sent
only to a host of that cloud. A `base_url` that only looks like one is
refused before any request is made. Commands the agent runs don't inherit
the credential: any variable ending in `_API_KEY`, `_TOKEN` or `_SECRET`,
and any `AWS_*` variable, is removed from their environment.

### Azure OpenAI (Foundry)

```json
{
  "provider": "azure",
  "base_url": "https://NAME.openai.azure.com/openai/v1",
  "model": "my-gpt-4o-deployment"
}
```

- **Key:** bwn reads it from `AZURE_OPENAI_API_KEY`, or from the key that
  `buildwithnexus init` saved. It is sent in the `api-key` header.
- **Model:** the name of your deployment.
- **Endpoint:** `base_url` is your resource's v1 endpoint, under
  `.openai.azure.com`, `.services.ai.azure.com` or
  `.cognitiveservices.azure.com`. It must use HTTPS on the default port,
  with no user name or password in the URL. Any other host gets no key.
- **API version:** the v1 API doesn't need an `api-version` parameter.

### Subscription sign-in

bwn doesn't offer sign-in with a Claude subscription. Anthropic's terms
for Claude Code say: "Anthropic does not permit third-party developers to
offer Claude.ai login into their own applications, or to route requests
through Free, Pro, Max, or Ultra plan credentials on behalf of their
users"
([legal and compliance](https://code.claude.com/docs/en/legal-and-compliance)).
Use an Anthropic API key, or Claude through your own cloud account.

### What was verified

The checks ran against tests, not live cloud accounts:

- The key binding: each host that should be refused is refused.
- The header that each endpoint receives.
