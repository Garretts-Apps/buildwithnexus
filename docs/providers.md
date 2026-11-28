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

### Claude on Vertex AI

```json
{
  "provider": "vertex",
  "base_url": "https://us-east5-aiplatform.googleapis.com/v1/projects/PROJECT/locations/us-east5",
  "model": "claude-sonnet-4-6"
}
```

- **Credentials:** Google's Application Default Credentials. bwn uses the
  file named by `GOOGLE_APPLICATION_CREDENTIALS`, or else the file that
  `gcloud auth application-default login` writes. The metadata server
  isn't used.
- **Service-account key:** bwn signs an RS256 assertion with the key and
  exchanges it for an access token.
- **User credential:** bwn exchanges its refresh token for an access
  token.
- **Token endpoint:** the exchange always goes to
  `https://oauth2.googleapis.com/token`. A `token_uri` in the file is
  ignored, so a planted file can't send a credential anywhere else.
- **Token storage:** the token is kept in memory only and renewed before
  it expires.
- **Endpoint:** `base_url` names your project and region, under
  `aiplatform.googleapis.com` or `REGION-aiplatform.googleapis.com`. A
  token goes to no other host. bwn checks the host before it reads any
  credential.
- **Requests:** bwn sends `streamRawPredict` (or `rawPredict`) for the
  model, with `anthropic_version: vertex-2023-10-16` in the body.
- **Hidden from commands:** the key file and gcloud's folder are hidden
  from the commands the agent runs, and so is the variable.

### Claude on Amazon Bedrock

```json
{
  "provider": "bedrock",
  "base_url": "https://bedrock-runtime.us-east-1.amazonaws.com",
  "model": "us.anthropic.claude-sonnet-4-6"
}
```

- **Bedrock API key:** with one in `AWS_BEARER_TOKEN_BEDROCK`, bwn sends
  it as a Bearer token.
- **Access key:** without an API key, bwn signs each request in-tree with
  Signature Version 4 (service `bedrock`). The key comes from
  `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`,
  or else from the `AWS_PROFILE` (or `default`) profile in
  `~/.aws/credentials`. The secret itself is never sent.
- **Not supported:** SSO and `credential_process` profiles.
- **Requests:** bwn calls `invoke`, or `invoke-with-response-stream` when
  it streams. The body is Anthropic's, with
  `anthropic_version: bedrock-2023-05-31` and no model or stream field.
- **Streamed replies:** each event-stream frame is checked by both of its
  CRCs, and a frame that fails either check ends the reply.
- **Throttling:** Bedrock's throttling reads as a busy model, so
  `fallback_model` takes over as it does with other providers.
- **Endpoint:** `base_url` must be
  `https://bedrock-runtime.REGION.amazonaws.com`. No other host gets the
  key or a signature.
- **Hidden from commands:** every `AWS_*` variable is stripped from
  commands. `~/.aws` and a file named by `AWS_SHARED_CREDENTIALS_FILE`
  are hidden stores.

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
- The Vertex request body.
- An assertion signed with a key generated for each test run, checked
  against that key's public half.
- A token exchange against a mock endpoint, with a planted `token_uri`
  that receives nothing.
- SigV4 signatures against AWS's published test suite (awslabs/aws-c-auth).
- Event-stream frames with corrupt checksums or oversized lengths.
- A streamed Bedrock reply and a throttled one.
