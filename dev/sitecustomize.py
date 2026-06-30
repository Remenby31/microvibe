import os

server_url = os.environ.get("VIBE_PARITY_MISTRAL_SERVER_URL")
if server_url:
    try:
        from mistralai.client import sdkconfiguration

        sdkconfiguration.SERVERS[sdkconfiguration.SERVER_EU] = server_url.rstrip("/")
    except Exception:
        pass

latest_version = os.environ.get("VIBE_PARITY_UPDATE_LATEST")
if latest_version:
    try:
        from vibe.cli.update_notifier.adapters.pypi_update_gateway import PyPIUpdateGateway
        from vibe.cli.update_notifier.ports.update_gateway import Update

        async def _fetch_update(self):
            return Update(latest_version=latest_version)

        PyPIUpdateGateway.fetch_update = _fetch_update
    except Exception:
        pass

if os.environ.get("VIBE_PARITY"):
    try:
        from vibe.core.telemetry.send import TelemetryClient

        def _send_onboarding_api_key_added(self):
            return None

        TelemetryClient.send_onboarding_api_key_added = _send_onboarding_api_key_added
    except Exception:
        pass

fake_clipboard = os.environ.get("MICROVIBE_FAKE_CLIPBOARD")
if os.environ.get("VIBE_PARITY") and fake_clipboard:
    try:
        import pyperclip

        def _copy(text):
            with open(fake_clipboard, "w", encoding="utf-8") as handle:
                handle.write(text)

        def _paste():
            try:
                with open(fake_clipboard, "r", encoding="utf-8") as handle:
                    return handle.read()
            except FileNotFoundError:
                return ""

        pyperclip.copy = _copy
        pyperclip.paste = _paste
    except Exception:
        pass
