import type { Page, TestInfo } from '@playwright/test'
import type { NextWorkerFixture, FetchHandler } from './next-worker-fixture'
import { handleRoute } from './page-route'

export interface NextFixture {
  onFetch: (handler: FetchHandler) => void
}

class NextFixtureImpl implements NextFixture {
  private fetchHandler: FetchHandler | null = null

  constructor(
    public testId: string,
    private worker: NextWorkerFixture,
    private page: Page
  ) {
    this.page.route('**', (route) =>
      handleRoute(route, page, this.fetchHandler)
    )
  }

  teardown(): void {
    this.worker.cleanupTest(this.testId)
  }

  onFetch(handler: FetchHandler): void {
    this.fetchHandler = handler
    this.worker.onFetch(this.testId, handler)
  }
}

export async function applyNextFixture(
  use: (fixture: NextFixture) => Promise<void>,
  {
    testInfo,
    nextWorker,
    page,
    extraHTTPHeaders,
  }: {
    testInfo: TestInfo
    nextWorker: NextWorkerFixture
    page: Page
    extraHTTPHeaders: Record<string, string> | undefined
  }
): Promise<void> {
  const fixture = new NextFixtureImpl(testInfo.testId, nextWorker, page)
  page.setExtraHTTPHeaders({
    ...extraHTTPHeaders,
    'Next-Test-Proxy-Port': String(nextWorker.proxyPort),
    'Next-Test-Data': fixture.testId,
  })

  await use(fixture)

  fixture.teardown()
}
