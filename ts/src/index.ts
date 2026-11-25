/// Onboard a libID web wallet through the relayer: one relayed transaction
/// creates the wallet, links the identity with the genesis key as admin, binds
/// the identity privately, claims the handle's escrow into the wallet and
/// repays the relayer from it. Wire format: the relayer's README, "HTTP API".

import {
  authorizedData,
  bindOperation,
  calls,
  encodeOperationList,
  executeDigest,
  handleEscrowAbi,
  LINK_IDENTITY_DOMAIN,
  type SessionPolicy,
  walletFactoryAbi,
  walletRegistryAbi,
  walletSalt,
  walletTransactionData,
  webWalletAbi,
} from '@libid/contracts'
import {
  type Address,
  encodeAbiParameters,
  erc20Abi,
  type Hex,
  isAddressEqual,
  type PublicClient,
  parseAbiParameters,
  zeroAddress,
  zeroHash,
} from 'viem'
import { generatePrivateKey, privateKeyToAccount } from 'viem/accounts'

import {
  ClaimTooSmall,
  type Holding,
  type Holdings,
  quoteSwap,
  SWAP_ROUTES,
  type SwapPlan,
  type SwapRoute,
  shortfall,
  swapCalls,
  withSlippage,
} from './swap.js'

export * from './swap.js'

/// `GET /v1/config`, the fields the client reads.
export type RelayerConfig = {
  chainId: number
  relayer: Address
  factory: Address
  /// The wallets' `WalletRegistry`.
  registry: Address
  inclusionWindowSeconds: number
  quoteValiditySeconds: number
}

/// `POST /v1/quote`: sign `callGas` and a refund of at least the batch's value
/// plus `minRefund`, for a send at `gasLimit`, before `validUntil` (Unix seconds).
export type Quote = {
  callGas: number
  gasLimit: number
  maxFeePerGas: bigint
  l1FeeBound: bigint
  minRefund: bigint
  validUntil: number
}

/// A refusal: the HTTP status and the relayer's `{name, message, args}`.
export class RelayerError extends Error {
  constructor(
    readonly status: number,
    override readonly name: string,
    message: string,
    readonly args: Record<string, unknown> = {},
  ) {
    super(message)
  }
}

/// The genesis key, wherever it lives: `key` is its session key (an ECDSA
/// key's 20-byte address), `sign` returns its 65-byte signature of `digest`.
export type Signer = { key: Hex; sign(digest: Hex): Promise<Hex> }

/// What the ceremony's proof commits under `OPERATIONS_DOMAIN`: bind the
/// identity to `wallet`, and link it there with `key` under `policy`.
export function onboardingTransactionData(p: {
  chainId: number | bigint
  wallet: Address
  key: Hex
  policy: SessionPolicy
}): Hex {
  const link = walletTransactionData(p.chainId, p.wallet, p.wallet, p.key, p.policy)
  return encodeOperationList([
    bindOperation(p.wallet),
    { domain: LINK_IDENTITY_DOMAIN, data: link },
  ])
}

/// The wallet `factory` creates for the identity at `salt`.
export function walletAddress(
  client: PublicClient,
  factory: Address,
  platformId: Hex,
  idNode: Hex,
  salt = 0n,
): Promise<Address> {
  return client.readContract({
    address: factory,
    abi: walletFactoryAbi,
    functionName: 'walletAddress',
    args: [platformId, idNode, salt],
  })
}

export type Execution = { target: Address; value: bigint; callData: Hex }

const EXECUTIONS = parseAbiParameters('(address target, uint256 value, bytes callData)[]')

/// `executionCalldata` of an `AUTHORIZED_MODE` batch: `abi.encode(Execution[])`.
export function encodeExecutions(executions: readonly Execution[]): Hex {
  return encodeAbiParameters(EXECUTIONS, [executions])
}

export type Onboarding = {
  relayerUrl: string
  client: PublicClient
  proof: { platformId: Hex; verifierVersion: number; payload: Hex }
  idNode: Hex
  salt?: bigint
  signer: Signer
  policy: SessionPolicy
  identityRegistry: Address
  escrow: Address
  handleNode: Hex
  /// What the handle holds: `indexerHoldings(url)`, or `escrowHoldings` from the chain.
  holdings: Holdings
  /// Approved tokens by chain id; `SWAP_ROUTES` by default.
  routes?: Record<number, readonly SwapRoute[]>
  /// The most a swap may pay over its quote, in basis points; 100 by default.
  slippageBps?: number
}

/// Link, bind privately, claim `tokens` into the wallet, swap when `swap` is
/// set, then repay `refund` to the relayer.
export function onboardingBatch(
  o: Pick<Onboarding, 'proof' | 'policy' | 'identityRegistry' | 'escrow' | 'handleNode'>,
  b: { wallet: Address; fee: bigint; relayer: Address; tokens: readonly Address[] },
  refund: bigint,
  deadline: number,
  swap?: SwapPlan,
): Execution[] {
  const { platformId, verifierVersion, payload } = o.proof
  const { wallet } = b
  const swaps = swap ? swapCalls(swap, wallet, deadline) : []
  return [
    calls.webWallet.linkIdentity(wallet, b.fee, platformId, verifierVersion, payload, o.policy),
    calls.identityRegistry.bindPrivately(
      o.identityRegistry,
      0n,
      platformId,
      verifierVersion,
      payload,
      0n,
      zeroAddress,
    ),
    calls.handleEscrow.claim(o.escrow, o.handleNode, b.tokens, wallet),
    ...swaps,
    { to: b.relayer, value: refund, data: '0x' as Hex },
  ].map((c) => ({ target: c.to, value: c.value ?? 0n, callData: c.data }))
}

/// `GET /v1/config`: among others, the factory `walletAddress` reads.
export async function relayerConfig(relayerUrl: string): Promise<RelayerConfig> {
  return (await relayerHttp(relayerUrl)('GET', '/v1/config')) as RelayerConfig
}

/// A relayed onboarding: the request, the wallet, the quote it was signed
/// under, and what the refund is made of.
export type Onboarded = {
  id: Hex
  wallet: Address
  /// The genesis session key, admin once the onboarding lands.
  key: Hex
  quote: Quote
  /// `HandleEscrow.NATIVE`, the chain's coin among `tokens`.
  native: Address
  /// The tokens the batch claims into the wallet.
  tokens: Address[]
  linkFee: bigint
  gasRefund: bigint
  /// Each swap's tokens in, and the least ETH it must return.
  swapped: { token: Address; amountIn: bigint; ethOutMin: bigint }[]
}

/// Quote, sign and relay the onboarding. Returns the request's id, the wallet,
/// and the quote it was signed under; `waitFor` follows it on chain. Throws
/// `ClaimTooSmall` when the holdings cannot repay the relayer.
export async function onboard(o: Onboarding): Promise<Onboarded> {
  const http = relayerHttp(o.relayerUrl)
  const config = await relayerConfig(o.relayerUrl)
  const chainId = await o.client.getChainId()
  if (chainId !== config.chainId) {
    throw new Error(`the client is on chain ${chainId}, the relayer on ${config.chainId}`)
  }
  const { platformId, verifierVersion, payload } = o.proof
  const salt = o.salt ?? 0n
  const native = { address: o.escrow, abi: handleEscrowAbi, functionName: 'NATIVE' } as const
  const [wallet, [, version], fee, block, coin, holdings] = await Promise.all([
    walletAddress(o.client, config.factory, platformId, o.idNode, salt),
    o.client.readContract({
      address: config.factory,
      abi: walletFactoryAbi,
      functionName: 'latest',
    }),
    o.client.readContract({
      address: config.registry,
      abi: walletRegistryAbi,
      functionName: 'quoteFor',
      args: [platformId, verifierVersion, payload],
    }),
    o.client.getBlock(),
    o.client.readContract(native),
    o.holdings(o.handleNode, chainId),
  ])
  const heldOf = (token: Address) =>
    holdings.find((h) => isAddressEqual(h.token, token) && h.amount > 0n)?.amount ?? 0n
  const eth = heldOf(coin)
  // Every approved token held is claimed; the one worth the most ETH to the
  // quoter pays the swap.
  const valued = ((o.routes ?? SWAP_ROUTES)[chainId] ?? [])
    .map((route) => ({ route, held: heldOf(route.token) }))
    .filter((r) => r.held > 0n)
    .map(async (r) => ({ ...r, worth: await quoteSwap(o.client, r.route, { in: r.held }) }))
  const approved = await Promise.all(valued)
  const best = approved.filter((r) => r.worth > 0n).sort((x, y) => (y.worth > x.worth ? 1 : -1))[0]
  const tokens = [...(eth > 0n ? [coin] : []), ...approved.map((r) => r.route.token)]
  if (tokens.length === 0) throw new ClaimTooSmall(0n, fee)
  const b = { wallet, fee, relayer: config.relayer, tokens }

  const at = Number(block.timestamp)
  // A wallet not yet created signs at nonce 0 under the domain salt of epoch 0.
  const authorize = async (
    refund: bigint,
    callGas: number,
    deadline: number,
    signer: Signer,
    swap?: SwapPlan,
  ) => {
    const batch = onboardingBatch(o, b, refund, deadline, swap)
    const message = {
      executionCalldata: encodeExecutions(batch),
      value: fee,
      at,
      nonce: 0n,
      deadline,
      callGas: BigInt(callGas),
    }
    const digest = executeDigest(message, chainId, wallet, version, walletSalt(zeroHash, 0))
    const signature = await signer.sign(digest)
    const signatures = [{ key: o.signer.key, signature }]
    return authorizedData(message, { signatures, calls: [], domainProof: [], paymasterProof: [] })
  }
  const create = { platformId, idNode: o.idNode, salt: salt.toString() }
  // The draft: well-formed, signed by a throwaway key under the real key bytes.
  const quoted = async (swap?: SwapPlan) => {
    const data = await authorize(0n, 0, at + 3600, throwaway(), swap)
    return parseQuote(await http('POST', '/v1/quote', { wallet, create, data }))
  }
  // The swap of the best token that returns at least what the claimed ETH lacks.
  const planned = async (refund: bigint): Promise<SwapPlan | undefined> => {
    const minOut = shortfall(refund, eth)
    if (minOut === 0n) return undefined
    if (!best) throw new ClaimTooSmall(eth, refund)
    const quotedIn = await quoteSwap(o.client, best.route, { out: minOut })
    const amountIn = withSlippage(quotedIn, o.slippageBps ?? 100)
    if (amountIn > best.held) throw new ClaimTooSmall(eth + best.worth, refund)
    return { route: best.route, amountIn, minOut }
  }
  // A quote, and the swap its refund needs. Quoted without a swap first; a
  // shortfall drafts the swap and quotes again, so only a needed swap is priced.
  const priced = async (drafted?: SwapPlan) => {
    let quote = await quoted(drafted)
    let swap = await planned(fee + quote.minRefund)
    if (swap && !drafted) {
      quote = await quoted(swap)
      swap = await planned(fee + quote.minRefund)
    }
    return { quote, swap }
  }
  let { quote, swap } = await priced()
  for (let retried = false; ; retried = true) {
    const refund = fee + quote.minRefund
    const deadline = quote.validUntil + config.inclusionWindowSeconds
    const data = await authorize(refund, quote.callGas, deadline, o.signer, swap)
    try {
      const body = { wallet, create, data, gasLimit: quote.gasLimit }
      const { id } = (await http('POST', '/v1/relay', body)) as { id: Hex }
      const swapped = swap
        ? [{ token: swap.route.token, amountIn: swap.amountIn, ethOutMin: swap.minOut }]
        : []
      const parts = { linkFee: fee, gasRefund: quote.minRefund, swapped }
      return { id, wallet, key: o.signer.key, quote, native: coin, tokens, ...parts }
    } catch (e) {
      if (retried || !(e instanceof RelayerError) || e.name !== 'GasLimitTooLow') throw e
      ;({ quote, swap } = await priced(swap))
    }
  }
}

type WaitOptions = {
  intervalMs?: number
  timeoutMs?: number
  client?: PublicClient
  onboarded?: Pick<Onboarded, 'wallet' | 'key' | 'native' | 'tokens'>
}

/// `GET /v1/relay/{id}` every `intervalMs` until the request settles. An
/// inclusion that reverted, a failure and a cancellation throw. With `client`
/// and an onboarding, also what the new wallet holds of its claimed tokens at
/// the inclusion block: what it received, net of the swap and the refund.
/// A request the relayer dropped, past its receipt's lag, is `UnknownRequest`:
/// with `client` and an onboarding the chain answers instead, settled once the
/// wallet has code and the genesis key is its admin session, at the latest
/// block, with no hashes.
export async function waitFor(
  relayerUrl: string,
  id: Hex,
  { intervalMs = 500, timeoutMs = 120_000, client, onboarded }: WaitOptions = {},
): Promise<{ txHashes: Hex[]; blockNumber: number; received: Holding[] }> {
  const http = relayerHttp(relayerUrl)
  for (const end = Date.now() + timeoutMs; Date.now() < end; ) {
    const s = await (http('GET', `/v1/relay/${id}`) as Promise<RelayStatus>).catch(
      async (e): Promise<RelayStatus> => {
        const unknown = e instanceof RelayerError && e.name === 'UnknownRequest'
        if (!unknown || !client || !onboarded) throw e
        const at = await landedAt(client, onboarded)
        return { status: at === undefined ? 'sent' : 'included', txHashes: [], blockNumber: at }
      },
    )
    if (s.status === 'included' && !s.reverted) {
      const blockNumber = s.blockNumber as number
      const received =
        client && onboarded ? await balances(client, onboarded, BigInt(blockNumber)) : []
      return { txHashes: s.txHashes, blockNumber, received }
    }
    if (s.status === 'included') throw new RelayerError(200, 'Reverted', 'the batch reverted')
    if (s.status === 'failed') throw refusal(200, s.error)
    if (s.status === 'cancelled')
      throw new RelayerError(200, 'Cancelled', 'the relayer cancelled it')
    await new Promise((r) => setTimeout(r, intervalMs))
  }
  throw new Error(`request ${id} did not settle in ${timeoutMs} ms`)
}

/// The latest block once `wallet` has code and `key` is its admin session.
async function landedAt(
  client: PublicClient,
  { wallet, key }: Pick<Onboarded, 'wallet' | 'key'>,
): Promise<number | undefined> {
  const blockNumber = await client.getBlockNumber()
  const code = await client.getCode({ address: wallet, blockNumber })
  if (!code || code === '0x') return undefined
  const read = { address: wallet, abi: webWalletAbi, functionName: 'sessionOf' } as const
  const session = await client.readContract({ ...read, args: [key], blockNumber })
  return session.admin ? Number(blockNumber) : undefined
}

/// `wallet`'s balance of ETH (`native`) and of each of `tokens` at `blockNumber`.
function balances(
  client: PublicClient,
  { wallet, native, tokens }: Pick<Onboarded, 'wallet' | 'native' | 'tokens'>,
  blockNumber: bigint,
): Promise<Holding[]> {
  const of = (token: Address) =>
    isAddressEqual(token, native)
      ? client.getBalance({ address: wallet, blockNumber })
      : client.readContract({
          address: token,
          abi: erc20Abi,
          functionName: 'balanceOf',
          args: [wallet],
          blockNumber,
        })
  const all = [native, ...tokens.filter((t) => !isAddressEqual(t, native))]
  return Promise.all(all.map(async (token) => ({ token, amount: await of(token) })))
}

type RelayStatus = {
  status: 'queued' | 'sent' | 'included' | 'cancelled' | 'failed'
  txHashes: Hex[]
  blockNumber?: number
  reverted?: boolean
  error?: unknown
}

function throwaway(): Signer {
  const account = privateKeyToAccount(generatePrivateKey())
  return { key: account.address, sign: (hash) => account.sign({ hash }) }
}

function parseQuote(q: unknown): Quote {
  const r = q as Record<string, string | number>
  return {
    callGas: Number(r.callGas),
    gasLimit: Number(r.gasLimit),
    maxFeePerGas: BigInt(r.maxFeePerGas),
    l1FeeBound: BigInt(r.l1FeeBound),
    minRefund: BigInt(r.minRefund),
    validUntil: Number(r.validUntil),
  }
}

/// A refusal body's `{name, message, args?}` as a `RelayerError`.
export function refusal(status: number, error: unknown): RelayerError {
  const e = (error ?? {}) as { name?: string; message?: string; args?: Record<string, unknown> }
  return new RelayerError(status, e.name ?? 'Unknown', e.message ?? `HTTP ${status}`, e.args)
}

/// JSON over `fetch`; a non-2xx answer throws its `RelayerError`.
function relayerHttp(base: string) {
  return async (method: 'GET' | 'POST', path: string, body?: unknown): Promise<unknown> => {
    const response = await fetch(new URL(path, base), {
      method,
      headers: body === undefined ? {} : { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    })
    const text = await response.text()
    const json =
      text && response.headers.get('content-type')?.includes('json') ? JSON.parse(text) : {}
    if (!response.ok) throw refusal(response.status, json.error ?? { message: text || undefined })
    return json
  }
}
