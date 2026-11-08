const assert = require('node:assert/strict');
const { chromium } = require('playwright');
const { pathToFileURL } = require('node:url');
const path = require('node:path');
const fs = require('node:fs');
(async () => {
  const browser = await chromium.launch({channel:'chrome',headless:true});
  try {
    const page = await browser.newPage({viewport:{width:1440,height:1080}});
    const errors=[]; page.on('pageerror',e=>errors.push(e.message));
    await page.goto(pathToFileURL(path.resolve(__dirname,'../docs/desktop-reminders-prototype.html')).href);
    assert(await page.locator('#panel img').evaluate(i=>i.complete&&i.naturalWidth>0));
    const desktop = () => page.locator('#desktop').evaluate(e=>({x:e.getBoundingClientRect().x+e.clientLeft,y:e.getBoundingClientRect().y+e.clientTop,w:e.clientWidth,h:e.clientHeight}));
    async function dragTo(id,x,y) {
      const box=await page.locator('#'+id).boundingBox();const area=await desktop();
      const hx=id==='panel'?70:40,hy=id==='panel'?25:40;
      await page.mouse.move(box.x+hx,box.y+hy);await page.mouse.down();
      await page.mouse.move(area.x+x+hx,area.y+y+hy,{steps:40});await page.mouse.up();
    }
    // Real continuous pointer movement (not synthetic dispatch): regression for native image dragging.
    await dragTo('pet',700,80);
    assert.equal(Math.round(await page.locator('#pet').evaluate(e=>e.offsetLeft)),700);
    await dragTo('pet',100,300);
    assert.equal(Math.round(await page.locator('#pet').evaluate(e=>e.offsetLeft)),100);
    await dragTo('panel',400,100);
    assert.equal(Math.round(await page.locator('#panel').evaluate(e=>e.offsetLeft)),400);
    await page.mouse.move(1000,300);
    assert.notEqual(await page.locator('#pet').evaluate(e=>e.style.getPropertyValue('--tilt-x')),'0deg');
    for(const [id,tab,mode] of [['panel','edge','panelOnly'],['pet','petEdge','petOnly']]) {
      await page.locator('#'+mode).click();
      for(const side of ['left','right','top','bottom']) {
        await page.locator('#reset').click();
        await page.getByLabel('模拟任务栏位置').selectOption(side==='bottom'?'top':'bottom');
        const area=await desktop(),box=await page.locator('#'+id).boundingBox();
        const x=side==='left'?0:side==='right'?area.w-box.width:400;
        const y=side==='top'?0:side==='bottom'?area.h-box.height:100;
        await dragTo(id,x,y);
        assert.equal(await page.locator('#'+id).getAttribute('data-edge'),side);
        // Hover keeps the newly docked entry open; leaving alone must retract it.
        await page.mouse.move(20,20);
        await page.waitForFunction(id=>document.getElementById(id).dataset.docked==='true',id);
        await page.waitForTimeout(260);
        assert.equal(await page.locator('#'+tab).getAttribute('data-edge'),side);
        await page.locator('#'+tab).hover();
        await page.waitForFunction(id=>document.getElementById(id).dataset.docked==='false',id);
        await page.waitForTimeout(260);
        // Moving taskbar onto the dock edge must detach and expose the entry.
        await page.getByLabel('模拟任务栏位置').selectOption(side);
        assert.equal(await page.locator('#'+id).getAttribute('data-edge'),'');
        assert(!await page.locator('#'+id).evaluate(e=>e.hidden));
      }
      // All four occupied taskbar edges reject docking.
      for(const side of ['left','right','top','bottom']) {
        await page.locator('#reset').click();await page.getByLabel('模拟任务栏位置').selectOption(side);
        const area=await desktop(),box=await page.locator('#'+id).boundingBox();
        await dragTo(id,side==='left'?38:side==='right'?area.w-38-box.width:400,
          side==='top'?38:side==='bottom'?area.h-38-box.height:100);
        assert.equal(await page.locator('#'+id).getAttribute('data-edge'),'');
      }
    }
    await page.getByLabel('模拟任务栏位置').selectOption('bottom');await page.locator('#both').click();await page.locator('#reset').click();
    await page.locator('#pin').click();await dragTo('panel',0,80);await page.waitForTimeout(900);
    assert.equal(await page.locator('#panel').getAttribute('data-docked'),'false');
    await page.locator('#pin').click();
    // Regression: mouse-clicked Pin retains focus, but must not block auto-hide.
    const outside=await desktop();await page.mouse.move(outside.x+outside.w/2,outside.y+outside.h-45);
    await page.waitForFunction(()=>document.querySelector('#panel').dataset.docked==='true',null,{timeout:1500});
    // Observe a nonzero animation duration and an intermediate position.
    assert((await page.locator('#panel').evaluate(e=>getComputedStyle(e).transitionDuration)).includes('0.24s'));
    await page.waitForTimeout(260);
    await page.locator('#edge').hover();await page.waitForTimeout(140);
    const animated=await page.locator('#panel').evaluate(e=>new DOMMatrix(getComputedStyle(e).transform).m41);
    assert(animated<0&&animated>-340,'reveal should interpolate instead of teleporting');
    await page.waitForTimeout(260);
    await page.locator('#petOpen').click();await page.locator('#later').click();
    assert((await page.locator('#status').textContent()).includes('1 小时'));
    await page.locator('#empty').click();assert((await page.locator('#items').textContent()).includes('暂时没有'));
    assert.equal(await page.locator('.tab-count').textContent(),'0');
    await page.locator('#empty').click();await page.locator('#reset').click();
    // Closing removes only that entry; preview buttons restore it.
    assert.equal(await page.locator('#pin').textContent(),'');
    assert.equal(await page.locator('#panelClose').textContent(),'');
    await page.locator('#panelClose').click();assert(await page.locator('#panel').evaluate(e=>e.hidden));
    assert(await page.locator('#edge').evaluate(e=>e.hidden));
    assert(!await page.locator('#pet').evaluate(e=>e.hidden));
    await page.locator('#both').click();await page.locator('#reset').click();
    await page.mouse.move(20,20);await page.waitForTimeout(150);
    assert.equal(await page.locator('#petClose').evaluate(e=>getComputedStyle(e).opacity),'0');
    await page.locator('#petHandle').hover();await page.waitForTimeout(150);
    assert.equal(await page.locator('#petClose').evaluate(e=>getComputedStyle(e).opacity),'1');
    await page.locator('#petClose').click();assert(await page.locator('#pet').evaluate(e=>e.hidden));
    assert(await page.locator('#bubble').evaluate(e=>e.hidden));assert(await page.locator('#petEdge').evaluate(e=>e.hidden));
    assert(!await page.locator('#panel').evaluate(e=>e.hidden));
    await page.locator('#both').click();await page.locator('#reset').click();
    await page.locator('#petHandle').focus();await page.waitForTimeout(150);
    assert.equal(await page.locator('#petClose').evaluate(e=>getComputedStyle(e).opacity),'1');
    await page.keyboard.press('Tab');assert.equal(await page.evaluate(()=>document.activeElement.id),'petClose');
    await page.keyboard.press('Enter');assert(await page.locator('#pet').evaluate(e=>e.hidden));
    await page.locator('#both').click();await page.locator('#reset').click();
    const output=path.resolve(__dirname,'../.agent-sync/reminder-design-evidence');fs.mkdirSync(output,{recursive:true});
    await page.locator('#petHandle').hover();await page.waitForTimeout(150);
    await page.screenshot({path:path.join(output,'both-v3.png')});
    await page.locator('#theme').click();await page.screenshot({path:path.join(output,'dark-v3.png')});
    // Window resize leaves both entries inside the simulated work area.
    await page.setViewportSize({width:700,height:900});await page.waitForTimeout(150);
    const area=await desktop();
    for(const id of ['pet','panel']) {
      const b=await page.locator('#'+id).boundingBox();assert(b.x>=area.x&&b.x+b.width<=area.x+area.w+1);assert(b.y>=area.y&&b.y+b.height<=area.y+area.h-38+1);
    }
    await page.emulateMedia({reducedMotion:'reduce'});
    assert.equal(await page.locator('#panel').evaluate(e=>getComputedStyle(e).transitionDuration),'0s');
    assert.deepEqual(errors,[]);
    console.log('PASS desktop prototype: free drag, four-edge docking, taskbar exclusions, mouse-leave retract with retained Pin focus, smooth reveal, independent close/reopen, hover-only orb close, keyboard access, reduced motion and resize.');
  } finally { await browser.close(); }
})().catch(e=>{console.error(e);process.exitCode=1;});
