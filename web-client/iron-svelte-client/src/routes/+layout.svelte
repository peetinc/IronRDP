<script lang="ts">
    import { installLogForwarding } from '$lib/log-forward';

    // Installed at the root rather than in Login's onMount: Login unmounts on
    // connect, and a reconnect from the session page would otherwise never arm
    // the forwarder. Runs once, before any route mounts, so the earliest wasm
    // log lines are captured.
    installLogForwarding();
</script>

<slot />
