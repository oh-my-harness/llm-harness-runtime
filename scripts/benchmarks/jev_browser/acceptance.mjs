export async function prepareAcceptance(task, page) {
  let expectedStory = null;
  if (task.name === 'hacker-news') {
    expectedStory = await page.locator('tr.athing').first().getAttribute('id');
    if (!/^\d+$/.test(expectedStory ?? '')) throw new Error('Cannot identify initial top story');
  }
  const initial = { url: page.url(), expected_story_id: expectedStory };
  return {
    initial,
    // The evaluator is private to the harness, never passed to run() or either policy.
    async check(finalPage) {
      let upstreamPass = false, upstreamError = null;
      try { upstreamPass = Boolean(await task.check(finalPage)); }
      catch (error) { upstreamError = error.name; }
      const url = new URL(finalPage.url());
      let strictPass = upstreamPass;
      if (task.name === 'hacker-news')
        strictPass = url.protocol === 'https:' && url.hostname === 'news.ycombinator.com'
          && url.pathname === '/item' && url.searchParams.get('id') === expectedStory;
      if (task.name === 'mdn')
        strictPass = url.protocol === 'https:' && url.hostname === 'developer.mozilla.org'
          && url.pathname.replace(/\/$/, '') === '/en-US/docs/Web/JavaScript/Reference/Global_Objects/Array/map';
      if (task.name === 'wikipedia')
        strictPass = url.protocol === 'https:' && url.hostname === 'en.wikipedia.org'
          && url.pathname.replace(/\/$/, '') === '/wiki/Ada_Lovelace';
      return { upstream_pass: upstreamPass, upstream_error: upstreamError,
        strict_pass: strictPass, final_url: finalPage.url() };
    },
  };
}
