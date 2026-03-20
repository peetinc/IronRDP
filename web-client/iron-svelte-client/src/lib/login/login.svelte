<script lang="ts">
    import { currentSession, setCurrentSessionActive, userInteractionService } from '../../services/session.service';
    import type { IronError, UserInteraction } from '../../../static/iron-remote-desktop';
    import type { Session } from '../../models/session';
    import { displayControl, kdcProxyUrl, init } from '../../../static/iron-remote-desktop-rdp';
    import { toast } from '$lib/messages/message-store';
    import { showLogin } from '$lib/login/login-store';
    import { onMount } from 'svelte';

    let username = 'Administrator';
    let password = '';
    let gatewayAddress = 'ws://localhost:8765';
    let hostname = 'localhost:3389';
    let domain = '';
    let kdc_proxy_url = '';
    let desktopSize = { width: 1280, height: 720 };
    let pop_up = false;
    let enable_clipboard = true;

    let userInteraction: UserInteraction;

    userInteractionService.subscribe((val) => {
        userInteraction = val;
    });

    const isIronError = (error: unknown): error is IronError => {
        return (
            typeof error === 'object' &&
            error !== null &&
            typeof (error as IronError).backtrace === 'function' &&
            typeof (error as IronError).kind === 'function'
        );
    };

    const StartSession = async () => {
        toast.set({
            type: 'info',
            message: 'Connection in progress...',
        });

        if (pop_up) {
            const data = JSON.stringify({
                username,
                password,
                hostname,
                gatewayAddress,
                domain,
                desktopSize,
                kdc_proxy_url,
                enable_clipboard,
            });
            const base64Data = btoa(data);
            window.open(
                `/popup-session?data=${base64Data}`,
                '_blank',
                `width=${desktopSize.width},height=${desktopSize.height},resizable=yes,scrollbars=yes,status=yes`,
            );
            return;
        }

        userInteraction.setEnableClipboard(enable_clipboard);

        const configBuilder = userInteraction
            .configBuilder()
            .withUsername(username)
            .withPassword(password)
            .withDestination(hostname)
            .withProxyAddress(gatewayAddress)
            .withServerDomain(domain)
            .withAuthToken('')
            .withDesktopSize(desktopSize)
            .withExtension(displayControl(true));

        if (kdc_proxy_url !== '') {
            configBuilder.withExtension(kdcProxyUrl(kdc_proxy_url));
        }

        const config = configBuilder.build();

        try {
            const session_info = await userInteraction.connect(config);

            toast.set({
                type: 'info',
                message: 'Success',
            });

            const updater = (session: Session): Session => ({
                ...session,
                sessionId: session_info.sessionId,
                desktopSize: session_info.initialDesktopSize,
                active: true,
            });

            currentSession.update(updater);

            showLogin.set(false);

            userInteraction.setVisibility(true);

            const sessionTerminationInfo = await session_info.run();

            toast.set({
                type: 'info',
                message: `Session terminated gracefully: ${sessionTerminationInfo.reason()}`,
            });
        } catch (err) {
            setCurrentSessionActive(false);
            showLogin.set(true);

            if (isIronError(err)) {
                toast.set({
                    type: 'error',
                    message: err.backtrace(),
                });
            } else {
                toast.set({
                    type: 'error',
                    message: `${err}`,
                });
            }
        }
    };

    onMount(async () => {
        await init('INFO');
    });
</script>

<main class="responsive login-container">
    <div class="login-content">
        <div class="grid">
            <div class="s2" />
            <div class="s8">
                <article class="primary-container">
                    <h5>Login</h5>
                    <div class="medium-space" />
                    <div>
                        <div class="field label border">
                            <input id="hostname" type="text" bind:value={hostname} />
                            <label for="hostname">Hostname</label>
                        </div>
                        <div class="field label border">
                            <input id="domain" type="text" bind:value={domain} />
                            <label for="domain">Domain</label>
                        </div>
                        <div class="field label border">
                            <input id="username" type="text" bind:value={username} />
                            <label for="username">Username</label>
                        </div>
                        <div class="field label border">
                            <input id="password" type="password" bind:value={password} />
                            <label for="password">Password</label>
                        </div>
                        <div class="field label border">
                            <input id="gatewayAddress" type="text" bind:value={gatewayAddress} />
                            <label for="gatewayAddress">Gateway Address</label>
                        </div>
                        <div class="field label border">
                            <input id="desktopSizeW" type="text" bind:value={desktopSize.width} />
                            <label for="desktopSizeW">Desktop Width</label>
                        </div>
                        <div class="field label border">
                            <input id="desktopSizeH" type="text" bind:value={desktopSize.height} />
                            <label for="desktopSizeH">Desktop Height</label>
                        </div>
                        <div class="field label border">
                            <input id="kdc_proxy_url" type="text" bind:value={kdc_proxy_url} />
                            <label for="kdc_proxy_url">KDC Proxy URL</label>
                        </div>
                        <div class="field label border checkbox-container">
                            <div class="checkbox-wrapper">
                                <input
                                    id="use_pop_up"
                                    type="checkbox"
                                    bind:checked={pop_up}
                                    style="width: 1.5em; height: 1.5em; margin-right: 0.5em;"
                                />
                                <label for="use_pop_up">Use Pop Up</label>
                            </div>
                            <div class="checkbox-wrapper">
                                <input
                                    id="enable_clipboard"
                                    type="checkbox"
                                    bind:checked={enable_clipboard}
                                    style="width: 1.5em; height: 1.5em; margin-right: 0.5em;"
                                />
                                <label for="enable_clipboard">Enable Clipboard</label>
                            </div>
                        </div>
                    </div>
                    <nav class="center-align">
                        <button on:click={StartSession}>Login</button>
                    </nav>
                </article>
            </div>
            <div class="s2" />
        </div>
    </div>
</main>

<style>
    @import './login.css';
</style>
