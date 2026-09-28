// Run with Playwright MCP browser_run_code_unsafe({ filename: <this file> }).
// Precondition: authenticated dashboard on loopback with synthetic state only,
// two available projects and a 96-character slug in /api/projects/overview.
// Set sessionStorage['rtrt.responsive-test'] to JSON { evidenceDir, phase } first.
// evidenceDir must exist; phase distinguishes before/after screenshot sets.
// Results remain in sessionStorage['rtrt.responsive-result'], including on failure.
// No browser launcher, npm dependencies, API mocks, or source-string assertions.
async (page) => {
  const { origin, hostname } = await page.evaluate(() => ({ origin: location.origin, hostname: location.hostname }));
  if (!['127.0.0.1', '[::1]', 'localhost'].includes(hostname)) {
    throw new Error('Use an isolated loopback dashboard with synthetic state.');
  }
  const config = await page.evaluate(() => JSON.parse(sessionStorage.getItem('rtrt.responsive-test')));
  if (!config?.evidenceDir || !/^[a-z0-9-]+$/.test(config.phase)) {
    throw new Error('Set rtrt.responsive-test to { evidenceDir, phase }.');
  }
  const result = { phase: config.phase, viewports: [], consoleErrors: [], pageErrors: [], httpErrors: [], failures: [] };
  const check = (condition, message) => { if (!condition) result.failures.push(message); };
  const onConsole = message => { if (message.type() === 'error') result.consoleErrors.push(message.text()); };
  const onError = error => result.pageErrors.push(error.message);
  const onResponse = response => {
    if (response.status() >= 400) result.httpErrors.push({ status: response.status(), url: response.url() });
  };
  page.on('console', onConsole);
  page.on('pageerror', onError);
  page.on('response', onResponse);
  const screenshot = async (width, state) => {
    const path = `${config.evidenceDir}/${config.phase}-${width}-${state}.png`;
    await page.screenshot({ path, fullPage: true, animations: 'disabled' });
    return path;
  };
  try {
    for (const width of [390, 768, 1280, 375]) {
      await page.setViewportSize({ width, height: 844 });
      await page.evaluate(() => sessionStorage.removeItem('rtrt.project'));
      await page.goto(`${origin}/overview`, { timeout: 10000 });
      await page.waitForFunction(() => {
        const rows = document.querySelectorAll('#global-project-overview-body tr');
        const options = document.querySelectorAll('#project-selector option');
        return rows.length >= 3 && options.length >= 4;
      }, null, { timeout: 10000 });
      const measurements = await page.evaluate(() => {
        const rect = selector => {
          const r = document.querySelector(selector).getBoundingClientRect();
          return { x: r.x, y: r.y, width: r.width, height: r.height, right: r.right };
        };
        const root = document.documentElement;
        const card = document.getElementById('global-project-overview');
        return {
          viewport: root.clientWidth, document: root.scrollWidth, body: document.body.scrollWidth,
          rootOverflow: getComputedStyle(root).overflowX, bodyOverflow: getComputedStyle(document.body).overflowX,
          asideDisplay: getComputedStyle(document.querySelector('aside')).display,
          gridColumns: getComputedStyle(document.querySelector('.layout')).gridTemplateColumns,
          main: rect('main'), aside: rect('aside'), picker: rect('#project-selector'),
          table: rect('#global-project-overview table'), card: rect('#global-project-overview'),
          cardClientWidth: card.clientWidth, cardScrollWidth: card.scrollWidth,
          cardOverflow: getComputedStyle(card).overflowX,
          slugs: Array.from(document.querySelectorAll('#global-project-overview-body td:nth-child(2) code'), el => el.textContent),
          navVisible: Array.from(document.querySelectorAll('aside .mode-nav')).some(el => el.getClientRects().length > 0),
        };
      });
      const row = { width, measurements, screenshots: [await screenshot(width, 'global')] };
      result.viewports.push(row);
      const picker = page.locator('#project-selector');
      check(await picker.count() === 1, `${width}: exactly one #project-selector`);
      check(await page.getByLabel('Project', { exact: true }).count() === 1, `${width}: picker has its associated label`);
      check(measurements.slugs.some(slug => slug.length === 96), `${width}: full 96-character slug rendered`);
      check(measurements.document <= measurements.viewport, `${width}: document overflow ${measurements.document} > ${measurements.viewport}`);
      check(!['hidden', 'clip'].includes(measurements.rootOverflow) && !['hidden', 'clip'].includes(measurements.bodyOverflow), `${width}: document overflow must not be masked`);
      if (measurements.cardScrollWidth > measurements.cardClientWidth) {
        check(['auto', 'scroll'].includes(measurements.cardOverflow), `${width}: wide table data remains scrollable inside its card`);
        await page.locator('#global-project-overview').hover({ timeout: 10000 });
        await page.mouse.wheel(measurements.cardScrollWidth, 0);
        await page.waitForFunction(() => document.getElementById('global-project-overview').scrollLeft > 0, null, { timeout: 10000 });
        row.cardScrolled = await page.locator('#global-project-overview').evaluate(el => el.scrollLeft);
        row.screenshots.push(await screenshot(width, 'global-scrolled'));
        await page.mouse.wheel(-measurements.cardScrollWidth, 0);
        await page.waitForFunction(() => document.getElementById('global-project-overview').scrollLeft === 0, null, { timeout: 10000 });
      }
      if (width <= 720) check(!measurements.navVisible, `${width}: oversized sidebar navigation stays collapsed`);
      if (width === 1280) {
        check(Math.abs(measurements.aside.width - 250) < 1 && Math.abs(measurements.main.x - 250) < 1, '1280: unchanged 250px desktop sidebar');
        check(measurements.navVisible, '1280: desktop navigation remains visible');
      }
      const visible = await picker.isVisible();
      check(visible, `${width}: project selector must be visible (aside display: ${measurements.asideDisplay})`);
      if (!visible) continue;
      check(measurements.picker.x >= 0 && measurements.picker.right <= width, `${width}: entire picker inside viewport`);
      const available = await picker.locator('option:not([disabled])').evaluateAll(options => options.map(option => option.value).filter(Boolean));
      check(available.length >= 2, `${width}: two available project fixtures`);
      if (available.length < 2) continue;

      // Follow native tab order from the preceding topbar control, not DOM .focus()
      // on a potentially hidden select. Exercise its real keyboard change handler.
      await page.locator('#theme-toggle').focus();
      await page.keyboard.press('Tab');
      const focused = await picker.evaluate(el => el === document.activeElement);
      check(focused, `${width}: Tab reaches project picker`);
      row.screenshots.push(await screenshot(width, 'focus'));
      if (!focused) continue;
      const waitForProject = slug => page.waitForResponse(response =>
        response.url().startsWith(`${origin}/api/overview?project=${encodeURIComponent(slug)}&`) && response.ok(),
      { timeout: 10000 });
      const firstResponse = waitForProject(available[0]);
      await page.keyboard.press('ArrowDown');
      const first = await firstResponse;
      row.keyboardSelection = { project: available[0], status: first.status(), scopedProject: first.request().headers()['x-rtrt-project'] };
      check(await picker.inputValue() === available[0], `${width}: keyboard selects first available project`);
      check(first.request().headers()['x-rtrt-project'] === available[0], `${width}: keyboard selection scopes real API request`);
      await page.waitForFunction(() => document.querySelector('#page-overview h1').textContent === 'Token Savings Overview', null, { timeout: 10000 });

      // Open the native picker with a pointer, then commit another option.
      const secondResponse = waitForProject(available[1]);
      await picker.click({ timeout: 10000 });
      await page.keyboard.press('ArrowDown');
      await page.keyboard.press('Enter');
      const second = await secondResponse;
      row.pointerSelection = { project: available[1], status: second.status(), scopedProject: second.request().headers()['x-rtrt-project'] };
      check(await picker.inputValue() === available[1], `${width}: clicking picker permits changing project`);
      check(second.request().headers()['x-rtrt-project'] === available[1], `${width}: pointer selection scopes real API request`);
      check(await page.evaluate(() => new URL(location.href).searchParams.get('project')) === available[1], `${width}: URL tracks project change`);
      check(await page.evaluate(() => sessionStorage.getItem('rtrt.project')) === available[1], `${width}: selected project persists`);
      check(await page.locator('#global-project-overview').isHidden(), `${width}: project change leaves global overview`);
      row.selectedProject = available[1];
      await page.waitForFunction(slug => document.querySelector('#project-savings-tbl .project-name-cell')?.textContent === slug, available[1], { timeout: 10000 });
      row.selectedMeasurements = await page.evaluate(() => {
        const table = document.getElementById('project-savings-tbl');
        const card = table.closest('.card');
        const name = table.querySelector('.project-name-cell');
        const range = document.createRange();
        range.selectNodeContents(name);
        return {
          viewport: document.documentElement.clientWidth,
          document: document.documentElement.scrollWidth,
          body: document.body.scrollWidth,
          tableWidth: table.getBoundingClientRect().width,
          nameWidth: name.getBoundingClientRect().width,
          nameLines: new Set(Array.from(range.getClientRects(), rect => rect.top)).size,
          cardClientWidth: card.clientWidth, cardScrollWidth: card.scrollWidth,
          cardOverflow: getComputedStyle(card).overflowX,
          cellCount: table.querySelectorAll('tbody tr:first-child td').length,
        };
      });
      check(row.selectedMeasurements.document <= row.selectedMeasurements.viewport, `${width}: selected project overflow ${row.selectedMeasurements.document} > ${row.selectedMeasurements.viewport}`);
      // The synthetic fixture slug should wrap at most once, not into a vertical
      // strip of fragments while the adjacent numeric columns keep their width.
      check(row.selectedMeasurements.nameLines <= 2, `${width}: selected project name compressed into ${row.selectedMeasurements.nameLines} lines`);
      check(row.selectedMeasurements.cellCount === 6, `${width}: all six savings cells remain rendered`);
      row.screenshots.push(await screenshot(width, 'selected'));
      const savingsCard = page.locator('#page-overview .card').filter({ has: page.locator('#project-savings-tbl') });
      await savingsCard.scrollIntoViewIfNeeded();
      const cardPath = `${config.evidenceDir}/${config.phase}-${width}-selected-card.png`;
      await savingsCard.screenshot({ path: cardPath, animations: 'disabled' });
      row.screenshots.push(cardPath);
      if (row.selectedMeasurements.cardScrollWidth > row.selectedMeasurements.cardClientWidth) {
        check(['auto', 'scroll'].includes(row.selectedMeasurements.cardOverflow), `${width}: selected table scroll belongs to its card`);
        await savingsCard.hover();
        await page.mouse.wheel(row.selectedMeasurements.cardScrollWidth, 0);
        await page.waitForFunction(() => {
          const table = document.getElementById('project-savings-tbl');
          const card = table.closest('.card');
          return card.scrollLeft > 0 && table.querySelector('td:last-child').getBoundingClientRect().right <= card.getBoundingClientRect().right;
        }, null, { timeout: 10000 });
        row.selectedCardScrolled = await savingsCard.evaluate(el => el.scrollLeft);
        const scrolledPath = `${config.evidenceDir}/${config.phase}-${width}-selected-card-scrolled.png`;
        await savingsCard.screenshot({ path: scrolledPath, animations: 'disabled' });
        row.screenshots.push(scrolledPath);
      }
    }
    check(!result.consoleErrors.length, 'No browser console errors');
    check(!result.pageErrors.length, 'No uncaught page errors');
    check(!result.httpErrors.length, 'No HTTP errors');
  } catch (error) {
    result.failures.push(String(error));
  } finally {
    page.off('console', onConsole);
    page.off('pageerror', onError);
    page.off('response', onResponse);
    await page.evaluate(result => sessionStorage.setItem('rtrt.responsive-result', JSON.stringify(result)), result);
  }
  if (result.failures.length) throw new Error(JSON.stringify(result, null, 2));
  return result;
}
