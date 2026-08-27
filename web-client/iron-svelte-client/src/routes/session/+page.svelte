<script lang="ts">
    import Login from '$lib/login/login.svelte';
    import RemoteScreen from '$lib/remote-screen/remote-screen.svelte';
    import Message from '$lib/messages/message.svelte';
    import { showLogin } from '$lib/login/login-store';
    import type { PrintJobEntry } from '../../models/print-job';

    // Test-rig state for the RDPDR virtual printer. Lives on this page
    // instance (not a module-level store) so it does not leak across
    // sessions/tabs. Login unmounts on connect ({#if $showLogin}), so it
    // receives a plain callback prop rather than a two-way `bind:` — that
    // keeps job updates flowing to this page regardless of Login's mount
    // state, instead of relying on Svelte's post-destroy binding behavior.
    let printJobs: PrintJobEntry[] = [];

    function upsertPrintJob(fileId: number, patch: Partial<PrintJobEntry>) {
        const idx = printJobs.findIndex((job) => job.id === fileId);
        if (idx === -1) {
            printJobs = [...printJobs, { id: fileId, status: 'printing', ...patch }];
        } else {
            printJobs = printJobs.map((job, i) => (i === idx ? { ...job, ...patch } : job));
        }
    }
</script>

{#if $showLogin}
    <Login onPrintJobUpdate={upsertPrintJob} />
{/if}
<RemoteScreen {printJobs} />

<Message></Message>
