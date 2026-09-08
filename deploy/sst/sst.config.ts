/* eslint-disable @typescript-eslint/no-explicit-any -- SST config uses ambient $-globals */
/// <reference path="./.sst/platform/config.d.ts" />

/**
 * SST v4 app — the AWS-serverless deploy path for `smooth-operator`.
 *
 * The reusable resources (API Gateway WebSocket + Rust Lambda wiring +
 * DynamoDB single table + S3 blob bucket + S3 Vectors env wiring + gateway-key
 * secret + IAM links) now live in the shared **`@smooai/deploy`** package
 * (`SmooAI/deploy`), consumed here via the `SmoothAgentApi` construct. This
 * file keeps only the **app-specific** config: the app name/stage/removal
 * policy and the per-app Lambda artifact dir + model.
 *
 * NEVER deploy locally — see `README.md`. CI owns deploys. Verification here is
 * `npx tsc --noEmit` + synth, not `sst deploy`.
 *
 * ## The Rust-Lambda build seam
 * SST has no native Rust builder, so the Lambda bootstrap is built out-of-band
 * with `cargo lambda` (see `README.md`) into `ARTIFACT_DIR`, and the construct
 * points the `Function` at that prebuilt artifact directory with the
 * `provided.al2023` custom runtime on `arm64`.
 *
 * ## The S3 Vectors gap
 * SST v4 ships no native S3 Vectors component (the service went GA 2025-12).
 * The construct declares the intended vector bucket/index names and wires them
 * into the Lambda env; the bucket/index are provisioned out-of-band (see
 * `README.md`). The Lambda reads the bucket name + index prefix from env and
 * uses its `s3-vectors` adapter feature.
 */

// SST has no native Rust builder — this is the `cargo lambda` output dir holding
// the `bootstrap` artifact.
//
// cargo-lambda names that directory after the BINARY, and this crate's [[bin]]
// is named `bootstrap` — so it is `target/lambda/bootstrap`, not
// `target/lambda/<package-name>`. The package-named path was wrong and would
// have deployed an EMPTY function: SST points at a directory, so a path that
// does not exist is not an error here, just an empty artifact.
//
// CI overrides it with the dir it actually found, so a future rename cannot
// reintroduce the same silent failure.
const ARTIFACT_DIR = process.env.SMOOTH_LAMBDA_ARTIFACT_DIR ?? '../../rust/target/lambda/bootstrap';

export default $config({
    app(input) {
        return {
            name: 'smooth-operator',
            removal: input?.stage === 'production' ? 'retain' : 'remove',
            protect: ['production'].includes(input?.stage ?? ''),
            home: 'aws',
            providers: {
                aws: {
                    region: (process.env.AWS_REGION as any) ?? 'us-east-1',
                },
            },
        };
    },

    async run() {
        // DYNAMIC import, not a top-level one: `sst install` hard-fails a config
        // with top-level imports ("this is not allowed"), which meant this file
        // could not generate its own platform types — and so the README's
        // documented `pnpm typecheck` verify step could never pass.
        const { SmoothAgentApi } = await import('@smooai/deploy');

        // Everything reusable is in the shared construct; only the app-specific
        // artifact dir + model are passed here. The construct provisions the
        // DynamoDB single table, S3 blob bucket, S3 Vectors env wiring,
        // gateway-key secret, the API Gateway WebSocket + Rust Lambda routes
        // (`$connect`/`$disconnect`/`send_message`/`ping`/`$default`), and the
        // IAM links/permissions (ManageConnections post-back + s3vectors:*).
        const agent = new SmoothAgentApi('SmoothAgent', {
            artifactDir: ARTIFACT_DIR,
            model: 'claude-haiku-4-5',
        });

        return agent.outputs;
    },
});
