"""Exercise the shipped Settings forms via a paired mobile viewport and the real gateway."""
import json


def exercise(js, wait):
    def go(label):
        wait(lambda: js("""const label=arguments[0], btn=[...document.querySelectorAll('#settings button')]
          .find(b=>b.textContent.trim()===label && b.getClientRects().length && !b.disabled);
          if(!btn)return false; btn.click(); return true""", label))

    def fill(name, value):
        js("""const c=document.querySelector('#settings [name="'+arguments[0]+'"]');
        c.value=arguments[1]; c.dispatchEvent(new Event('input',{bubbles:true})); c.dispatchEvent(new Event('change',{bubbles:true}));""", name, value)

    def saved():
        wait(lambda: js("return document.querySelector('.phone-settings-status')?.textContent==='Saved on the host.'"))

    def settings(query):
        return js("const q=arguments[0]; return import('./device.js').then(async m=>m.settingsRequest(await m.deviceKey(),q))", query)

    js("document.querySelector('#home').click()")
    wait(lambda: js("return !document.querySelector('#catalog').hidden"))
    js("document.querySelector('#catalog .action-menu > summary').click()")
    assert js("return document.querySelector('#catalog .action-menu').open && document.querySelector('#open-phone-settings').getClientRects().length>0")
    js("document.querySelector('#open-phone-settings').click()")
    go("Providers & models")
    go("Add provider")
    fill("provider_id", "phone-ui")
    fill("name", "Phone UI fixture")
    js("window.dispatchEvent(new CustomEvent('ox-native-login-finished'))")
    assert js("return document.querySelector('#settings [name=name]').value==='Phone UI fixture'")
    fill("base_url", "https://phone.example.invalid/v1")
    fill("wire_api", "responses")
    fill("env_key", "PHONE_UI_TEST_KEY")
    fill("discovery", "false")
    js("document.querySelector('#settings details:last-of-type').open=true")
    go("Add static model")
    fill("id", "model-one")
    go("Save provider")
    saved()
    provider = next(p for p in settings({"action": "read_rows"})["registry"]["providers"] if p["id"] == "phone-ui")
    assert provider["models"][0]["id"]["value"] == "model-one"
    assert provider["fields"]["discovery"] == {"type": "bool", "value": False}
    go("← Settings")
    go("Credentials")
    canary = "isolated-form-entry-for-remote-credentials"
    js("""const f=[...document.querySelectorAll('#settings form')].find(f=>f.querySelector('h2')?.textContent==='PHONE_UI_TEST_KEY');
          f.querySelector('input').value=arguments[0]; f.requestSubmit();""", canary)
    saved()
    snapshot = settings({"action": "read_rows"})
    assert any(c["name"] == "PHONE_UI_TEST_KEY" and c["saved"] for c in snapshot["credentials"])
    assert canary not in json.dumps(snapshot) and js("""return !document.querySelector('#settings').textContent.includes(arguments[0]) &&
        !JSON.stringify(localStorage).includes(arguments[0]) && !JSON.stringify(sessionStorage).includes(arguments[0])""", canary)
    go("← Settings")
    go("Favorites")
    fill("alias", "phone-ui/model-one")
    go("Add to favorites")
    go("Save favorites")
    saved()
    assert "phone-ui/model-one" in settings({"action": "read_rows"})["favorites"]
    go("← Settings")
    go("Web search")
    fill("max_results", "6")
    go("Save search settings")
    saved()
    assert settings({"action": "read_rows"})["registry"]["search"]["max_results"]["value"] == 6
    go("← Settings")
    go("Phone appearance & input")
    fill("theme", "light")
    fill("font_size", "19")
    go("Save phone preferences")
    assert js("return document.documentElement.dataset.phoneTheme==='light' && getComputedStyle(document.documentElement).fontSize==='19px'")
    fill("theme", "dark")
    fill("font_size", "16")
    go("Save phone preferences")
    go("← Settings")
    go("Desktop appearance & input")
    wait(lambda: js("return !!document.querySelector('#settings [name=font_size]')"))
    fill("font_size", "18")
    go("Save desktop preferences")
    saved()
    assert settings({"action": "read_behavior"})["behavior"]["font_size"] == 18
    go("← Settings")
    go("Accounts")
    wait(lambda: js("return [...document.querySelectorAll('#settings button')].some(b=>b.disabled && b.textContent==='Sign in with ChatGPT')"))
    js("window.fixtureAccountButton=document.querySelector('#settings button'); window.dispatchEvent(new CustomEvent('ox-native-login-finished'))")
    wait(lambda: js("return !fixtureAccountButton.isConnected && [...document.querySelectorAll('#settings button')].some(b=>b.textContent==='Sign in with Claude')"))
    go("← Settings")
    go("Remote access")
    go("Pair another device")
    wait(lambda: js("return !!document.querySelector('.phone-settings-invite canvas')"))
    assert js("""return import('./vendor/jsQR-1.4.0.js').then(()=>{
       const c=document.querySelector('.phone-settings-invite canvas'), p=c.getContext('2d').getImageData(0,0,c.width,c.height);
       return window.jsQR(p.data,p.width,p.height).data.startsWith(location.origin+'/#pair=');})""")
    assert js("return document.querySelector('#settings').innerText.includes('Managed in OX Desktop') && document.documentElement.scrollWidth<=innerWidth")
    go("← Settings")
    go("Install & notifications")
    go("Set up notifications")
    wait(lambda: js("return document.querySelector('#notification-setup').open && !document.querySelector('#catalog').hidden"))
    print("PASS authenticated phone Settings forms, scoped key, provider/static model, favorites/search, phone/desktop appearance, device grant, new QR and notification navigation", flush=True)
