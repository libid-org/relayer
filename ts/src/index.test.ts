import { executeDigest, handleEscrowAbi, walletSalt } from '@libid/contracts'
import {
  type Address,
  decodeAbiParameters,
  decodeFunctionData,
  erc20Abi,
  type Hex,
  type PublicClient,
  parseAbiParameters,
  recoverAddress,
  zeroHash,
} from 'viem'
import { generatePrivateKey, privateKeyToAccount } from 'viem/accounts'
import { afterEach, describe, expect, test, vi } from 'vitest'

import { type Holding, type Onboarding, onboard, RelayerError, swapAbi, waitFor } from './index.js'

const a = (n: number) => `0x${n.toString(16).padStart(40, '0')}` as Address
const [FACTORY, REGISTRY, RELAYER, WALLET, TOKEN, ROUTER] = [a(1), a(2), a(3), a(4), a(7), a(8)]
const NATIVE: Address = '0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE'
const FEE = 7n
const NOW = 1_000
/// Token units per ETH: 3000 of a 6-decimal token.
const RATE = 3_000_000_000n
const ETHER = 10n ** 18n
const config = {
  chainId: 31337,
  relayer: RELAYER,
  factory: FACTORY,
  registry: REGISTRY,
  inclusionWindowSeconds: 5,
  quoteValiditySeconds: 10,
}
const quote = (callGas: number, minRefund = '1000') => ({
  relayer: RELAYER,
  callGas,
  gasLimit: 900_000,
  maxFeePerGas: '1',
  l1FeeBound: '0',
  minRefund,
  validUntil: NOW + 10,
})

const ID = `0x${'11'.repeat(32)}` as Hex
const client = {
  getChainId: async () => config.chainId,
  getBlock: async () => ({ timestamp: BigInt(NOW) }),
  readContract: async ({ functionName }: { functionName: string }) =>
    ({ walletAddress: WALLET, latest: [a(9), 1n], quoteFor: FEE, NATIVE, proofVerifier: a(10) })[
      functionName
    ],
  simulateContract: async (c: { functionName: string; args: [Record<string, bigint>] }) => {
    const [p] = c.args
    const answers: Record<string, () => unknown> = {
      quoteExactInputSingle: () => [(p.amountIn * ETHER) / RATE],
      quoteExactOutputSingle: () => [(p.amount * RATE + ETHER - 1n) / ETHER],
    }
    return { result: answers[c.functionName]() }
  },
} as unknown as PublicClient

const k = privateKeyToAccount(generatePrivateKey())
const onboarding = (held: Holding[]): Onboarding => ({
  relayerUrl: 'http://relayer.test',
  client,
  proof: { platformId: zeroHash, verifierVersion: 1, payload: '0x01' },
  idNode: ID,
  signer: { key: k.address, sign: (hash) => k.sign({ hash }) },
  policy: {
    validAfter: 0,
    validUntil: 0,
    admin: true,
    signers: 0,
    rules: { scopeRoot: zeroHash, domainsRoot: zeroHash, paymastersRoot: zeroHash, allowances: [] },
  },
  identityRegistry: a(5),
  escrow: a(6),
  handleNode: zeroHash,
  holdings: async () => held,
  routes: { 31337: [{ token: TOKEN, fee: 500, router: ROUTER, quoter: ROUTER, weth: a(11) }] },
})
const ethOnly = onboarding([{ token: NATIVE, amount: ETHER }])

/// Answers each request with the next `[status, body]` its path has, and records the bodies.
function relayer(answers: Record<string, [number, unknown][]>) {
  const sent: Record<string, Record<string, unknown>[]> = {}
  vi.stubGlobal('fetch', async (url: URL, init: RequestInit) => {
    const path = url.pathname
    if (init.body) {
      sent[path] = [...(sent[path] ?? []), JSON.parse(init.body as string)]
    }
    const [status, body] =
      path === '/v1/config' ? [200, config] : (answers[path].shift() ?? [500, 'no answer left'])
    return new Response(JSON.stringify(body), {
      status,
      headers: { 'content-type': 'application/json' },
    })
  })
  return sent
}

const DATA = parseAbiParameters('bytes, uint256, uint48, uint256, bytes')
const EXECUTIONS = parseAbiParameters('(address target, uint256 value, bytes callData)[]')
const AUTH = parseAbiParameters(
  '((bytes key, bytes signature)[] signatures, uint48 at, (uint256 maxValue, bytes32[] proof)[] calls, bytes32[] domainProof, bytes32[] paymasterProof)',
)
function decoded(data: Hex) {
  const [calldata, value, deadline, callGas, auth] = decodeAbiParameters(DATA, data)
  const [executions] = decodeAbiParameters(EXECUTIONS, calldata)
  const [{ signatures, at }] = decodeAbiParameters(AUTH, auth)
  const message = { executionCalldata: calldata, value, at, nonce: 0n, deadline, callGas }
  const digest = executeDigest(message, config.chainId, WALLET, 1n, walletSalt(zeroHash, 0))
  return { executions, value, deadline, callGas, signatures, digest }
}

/// The swap a batch carries: what it approves, buys and unwraps.
function swapOf(executions: readonly { callData: Hex }[]) {
  const approve = decodeFunctionData({ abi: erc20Abi, data: executions[3].callData })
  const multicall = decodeFunctionData({ abi: swapAbi, data: executions[4].callData })
  const [buy, unwrap] = (multicall.args[1] as Hex[]).map((data) =>
    decodeFunctionData({ abi: swapAbi, data }),
  )
  const bought = buy.args[0] as { amountIn: bigint }
  return { approve: approve.args, buy: bought, unwrap: unwrap.args }
}

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('onboard', () => {
  test('drafts at no call gas and no refund, then signs the quote', async () => {
    const sent = relayer({
      '/v1/quote': [[200, quote(50_000)]],
      '/v1/relay': [[202, { id: '0xab', status: 'queued' }]],
    })
    const done = await onboard(ethOnly)
    expect(done).toMatchObject({ id: '0xab', wallet: WALLET, linkFee: FEE, gasRefund: 1000n })
    expect(done.swapped).toEqual([])

    const draft = decoded(sent['/v1/quote'][0].data as Hex)
    expect(draft.callGas).toBe(0n)
    expect(draft.executions.map((e) => [e.target, e.value])).toEqual([
      [WALLET, FEE],
      [a(5), 0n],
      [a(6), 0n],
      [RELAYER, 0n],
    ])
    const claim = decodeFunctionData({ abi: handleEscrowAbi, data: draft.executions[2].callData })
    expect(claim.args).toEqual([zeroHash, [NATIVE], WALLET])
    expect(draft.signatures[0].key).toBe(k.address.toLowerCase())
    const stranger = await recoverAddress({
      hash: draft.digest,
      signature: draft.signatures[0].signature,
    })
    expect(stranger).not.toBe(k.address)

    const relay = sent['/v1/relay'][0]
    expect(relay.gasLimit).toBe(900_000)
    expect(relay.create).toEqual({ platformId: zeroHash, idNode: ID, salt: '0' })
    const signed = decoded(relay.data as Hex)
    expect([signed.value, signed.callGas, signed.deadline]).toEqual([FEE, 50_000n, NOW + 15])
    expect(signed.executions[3]).toEqual({ target: RELAYER, value: FEE + 1000n, callData: '0x' })
    const signer = await recoverAddress({
      hash: signed.digest,
      signature: signed.signatures[0].signature,
    })
    expect(signer).toBe(k.address)
  })

  test('swaps the approved token for at least what the claimed ETH lacks', async () => {
    const priced: [number, unknown] = [200, quote(50_000, `${10n ** 15n}`)]
    const sent = relayer({
      '/v1/quote': [priced, priced],
      '/v1/relay': [[202, { id: '0xab', status: 'queued' }]],
    })
    const held = [
      { token: NATIVE, amount: 4n * 10n ** 14n },
      { token: TOKEN, amount: 5_000_000n },
    ]
    const done = await onboard(onboarding(held))

    // Quoted without a swap, then with the one the shortfall needs.
    const [plain, swapping] = sent['/v1/quote'].map((q) => decoded(q.data as Hex).executions)
    expect(plain.map((e) => e.target)).toEqual([WALLET, a(5), a(6), RELAYER])
    expect(swapping.map((e) => e.target)).toEqual([WALLET, a(5), a(6), TOKEN, ROUTER, RELAYER])

    // R = 7 + 10^15 wei, 4·10^14 claimed: sell 1800001 tokens and 1% for at
    // least 600000000000007 wei, approving exactly what the swap spends.
    const signed = swapOf(decoded(sent['/v1/relay'][0].data as Hex).executions)
    expect(signed.approve).toEqual([ROUTER, 1_818_002n])
    expect(signed.buy).toMatchObject({
      tokenIn: TOKEN,
      recipient: a(2),
      amountIn: 1_818_002n,
      amountOutMinimum: 600_000_000_000_007n,
    })
    expect(signed.unwrap).toEqual([600_000_000_000_007n, WALLET])
    expect(done.swapped).toEqual([
      { token: TOKEN, amountIn: 1_818_002n, ethOutMin: 600_000_000_000_007n },
    ])
    expect(done.tokens).toEqual([NATIVE, TOKEN])
  })

  test('refuses a claim that cannot repay, before relaying', async () => {
    const priced: [number, unknown] = [200, quote(50_000, `${10n ** 15n}`)]
    relayer({ '/v1/quote': [priced, priced], '/v1/relay': [] })
    await expect(onboard(onboarding([]))).rejects.toMatchObject({
      name: 'ClaimTooSmall',
      held: 0n,
      required: FEE,
    })
    const short = [
      { token: NATIVE, amount: 4n * 10n ** 14n },
      { token: TOKEN, amount: 1_000_000n },
    ]
    await expect(onboard(onboarding(short))).rejects.toMatchObject({
      name: 'ClaimTooSmall',
      held: 4n * 10n ** 14n + 333_333_333_333_333n,
      required: 10n ** 15n + FEE,
    })
    await expect(onboard(onboarding(short.slice(0, 1)))).rejects.toMatchObject({
      name: 'ClaimTooSmall',
      held: 4n * 10n ** 14n,
      required: 10n ** 15n + FEE,
    })
  })

  test("a proof of another identity is the quote's LinkSessionRefused", async () => {
    const reverted = { name: 'LinkSessionRefused', message: 'the batch reverts', args: {} }
    relayer({ '/v1/quote': [[400, { error: { ...reverted, contract: 'WebWallet' } }]] })
    const refused = await onboard(ethOnly).catch((e) => e)
    expect(refused).toBeInstanceOf(RelayerError)
    expect(refused).toMatchObject({ status: 400, name: 'LinkSessionRefused' })
  })

  test('quotes again once on GasLimitTooLow', async () => {
    const tooLow = [400, { error: { name: 'GasLimitTooLow', message: 'm', args: { required: 1 } } }]
    const sent = relayer({
      '/v1/quote': [
        [200, quote(50_000)],
        [200, quote(60_000)],
      ],
      '/v1/relay': [tooLow as [number, unknown], [202, { id: '0xcd', status: 'queued' }]],
    })
    const { quote: used } = await onboard(ethOnly)
    expect(used.callGas).toBe(60_000)
    expect(decoded(sent['/v1/relay'][1].data as Hex).callGas).toBe(60_000n)

    relayer({
      '/v1/quote': [
        [200, quote(50_000)],
        [200, quote(60_000)],
      ],
      '/v1/relay': [tooLow as [number, unknown], tooLow as [number, unknown]],
    })
    await expect(onboard(ethOnly)).rejects.toMatchObject({ name: 'GasLimitTooLow' })
  })

  test('a refusal is a RelayerError with its status, name and args', async () => {
    relayer({
      '/v1/quote': [[200, quote(1)]],
      '/v1/relay': [
        [409, { error: { name: 'IdentityBusy', message: 'busy', args: { id: '0xef' } } }],
      ],
    })
    const refused = await onboard(ethOnly).catch((e) => e)
    expect(refused).toBeInstanceOf(RelayerError)
    expect(refused).toMatchObject({
      status: 409,
      name: 'IdentityBusy',
      message: 'busy',
      args: { id: '0xef' },
    })
  })
})

describe('waitFor', () => {
  const unknown = [404, { error: { name: 'UnknownRequest', message: 'no such request' } }]
  /// A chain where the wallet has `code` and `key`'s session is `admin`.
  const chain = (code: Hex, admin: boolean) =>
    ({
      getBlockNumber: async () => 42n,
      getCode: async () => code,
      readContract: async () => ({ admin }),
      getBalance: async () => ETHER,
    }) as unknown as PublicClient
  const onboarded = { wallet: WALLET, key: k.address, native: NATIVE, tokens: [NATIVE] }
  const wait = (client: PublicClient) =>
    waitFor('http://relayer.test', ID, { intervalMs: 1, timeoutMs: 50, client, onboarded })

  test('once dropped, settles from the chain once the genesis key is admin', async () => {
    relayer({ [`/v1/relay/${ID}`]: [unknown as [number, unknown]] })
    await expect(wait(chain('0x01', true))).resolves.toEqual({
      txHashes: [],
      blockNumber: 42,
      received: [{ token: NATIVE, amount: ETHER }],
    })
  })

  test('once dropped, waits while the wallet has no code or no admin key', async () => {
    for (const client of [chain('0x', true), chain('0x01', false)]) {
      relayer({ [`/v1/relay/${ID}`]: Array(100).fill(unknown) })
      await expect(wait(client)).rejects.toThrow('did not settle')
    }
  })

  test('without a client, UnknownRequest throws', async () => {
    relayer({ [`/v1/relay/${ID}`]: [unknown as [number, unknown]] })
    await expect(waitFor('http://relayer.test', ID)).rejects.toMatchObject({
      name: 'UnknownRequest',
    })
  })
})
