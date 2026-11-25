/// Onboardings through the compose stack (README, "Local stack"): a fresh
/// identity, its handle's escrow claimed into a wallet that starts with no ETH,
/// the relayer repaid from it, in ETH or by swapping a token through a mock of
/// Uniswap's router. Skips without the stack; fails under CI.

import { execFileSync } from 'node:child_process'
import { mkdtempSync, readFileSync, realpathSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import {
  ceremonyProofVerifierAbi,
  handleEscrowAbi,
  OPERATIONS_DOMAIN,
  platformId,
  type SessionPolicy,
  walletRegistryAbi,
  webWalletAbi,
} from '@libid/contracts'
import {
  type Abi,
  type Address,
  createPublicClient,
  createWalletClient,
  encodeAbiParameters,
  erc20Abi,
  type Hex,
  http,
  parseAbiParameters,
  parseEther,
  type PublicClient,
  zeroHash,
} from 'viem'
import { generatePrivateKey, privateKeyToAccount } from 'viem/accounts'
import { foundry } from 'viem/chains'
import { afterAll, expect, test } from 'vitest'

import {
  type Holdings,
  onboard,
  onboardingTransactionData,
  relayerConfig,
  waitFor,
  walletAddress,
} from '../src/index.js'

const API = 'http://127.0.0.1:8080'
const ANVIL = 'http://127.0.0.1:8545'
const repo = join(import.meta.dirname, '..', '..')

// The stub's ABI as the pinned crate vendors it; its payload struct, which the ABI leaves out.
const contracts = join(realpathSync(join(repo, 'ts', 'vendor', 'contracts')), '..', '..', '..')
const stubAbi = JSON.parse(
  readFileSync(join(contracts, 'rust', 'contracts', 'abi', 'StubPlatformVerifier.json'), 'utf8'),
) as Abi
const STUB_PAYLOAD = parseAbiParameters(
  '(uint16 ceremonyVersion, bytes32 operationDomain, bytes32 authorizationNonce, bytes transactionData)',
)

const admin: SessionPolicy = {
  validAfter: 0,
  validUntil: 0,
  admin: true,
  signers: 0,
  rules: { scopeRoot: zeroHash, domainsRoot: zeroHash, paymastersRoot: zeroHash, allowances: [] },
}

/// `test/mocks/SwapMocks.sol`, compiled by forge into a scratch directory.
let mocks: string | undefined
afterAll(() => mocks && rmSync(mocks, { recursive: true, force: true }))
function mock(name: 'MockToken' | 'MockSwapRouter') {
  if (!mocks) {
    const out = mkdtempSync(join(tmpdir(), 'swap-mocks-'))
    const args = ['build', '--contracts', '.', '--out', out, '--cache-path', join(out, 'cache')]
    execFileSync('forge', args, { cwd: join(import.meta.dirname, 'mocks'), stdio: 'ignore' })
    mocks = out
  }
  const artifact = JSON.parse(readFileSync(join(mocks, 'SwapMocks.sol', `${name}.json`), 'utf8'))
  return { abi: artifact.abi as Abi, bytecode: artifact.bytecode.object as Hex }
}

/// `HandleEscrow.escrowed` of each of `tokens`, read from the chain.
const escrowHoldings =
  (client: PublicClient, escrow: Address, tokens: Address[]): Holdings =>
  (handleNode) =>
    Promise.all(
      tokens.map(async (token) => ({
        token,
        amount: await client.readContract({
          address: escrow,
          abi: handleEscrowAbi,
          functionName: 'escrowed',
          args: [handleNode, token],
        }),
      })),
    )

async function stack() {
  const ok = await fetch(`${API}/health`).catch(() => undefined)
  if (!ok) return undefined
  const addresses = JSON.parse(
    execFileSync('docker', ['compose', 'exec', '-T', 'relayer', 'cat', '/etc/relayer/stack.json'], {
      cwd: repo,
      encoding: 'utf8',
    }),
  ) as { identityRegistry: Address; escrow: Address }
  const client = createPublicClient({ chain: foundry, transport: http(ANVIL) })
  const [, , anyone] = await createWalletClient({ transport: http(ANVIL) }).getAddresses()
  const sender = createWalletClient({ account: anyone, transport: http(ANVIL), chain: foundry })
  const mined = async (hash: Hex) => {
    const receipt = await client.waitForTransactionReceipt({ hash })
    expect(receipt.status).toBe('success')
    return receipt
  }
  const config = await relayerConfig(API)
  const escrow = { address: addresses.escrow, abi: handleEscrowAbi } as const
  const native = await client.readContract({ ...escrow, functionName: 'NATIVE' })
  return { ...addresses, client, sender, mined, config, escrow, native }
}

/// A fresh identity, which the stack's stub, X's verifier 1, reports, its wallet
/// with no ETH, and K's proof that links it there as admin.
async function identity(s: NonNullable<Awaited<ReturnType<typeof stack>>>) {
  const platform = platformId('x')
  const [idNode, handleNode] = [generatePrivateKey(), generatePrivateKey()]
  const verifiers = await s.client.readContract({
    address: s.config.registry,
    abi: walletRegistryAbi,
    functionName: 'proofVerifier',
  })
  const stub = await s.client.readContract({
    address: verifiers,
    abi: ceremonyProofVerifierAbi,
    functionName: 'verifierOf',
    args: [platform, 1],
  })
  const write = {
    abi: stubAbi,
    address: stub,
    functionName: 'setNodes',
    args: [idNode, handleNode],
  }
  await s.mined(await s.sender.writeContract(write))
  const wallet = await walletAddress(s.client, s.config.factory, platform, idNode)
  expect(await s.client.getBalance({ address: wallet })).toBe(0n)
  const k = privateKeyToAccount(generatePrivateKey())
  const transactionData = onboardingTransactionData({
    chainId: s.config.chainId,
    wallet,
    key: k.address,
    policy: admin,
  })
  const payload = encodeAbiParameters(STUB_PAYLOAD, [
    {
      ceremonyVersion: 1,
      operationDomain: OPERATIONS_DOMAIN,
      authorizationNonce: generatePrivateKey(),
      transactionData,
    },
  ])
  const onboarding = {
    relayerUrl: API,
    client: s.client,
    proof: { platformId: platform, verifierVersion: 1, payload },
    idNode,
    signer: { key: k.address, sign: (hash: Hex) => k.sign({ hash }) },
    policy: admin,
    identityRegistry: s.identityRegistry,
    escrow: s.escrow.address,
    handleNode,
  }
  return { platform, handleNode, wallet, k, onboarding }
}

/// Onboard, wait, and check the escrow is empty and K is admin.
async function onboarded(
  s: NonNullable<Awaited<ReturnType<typeof stack>>>,
  o: Awaited<ReturnType<typeof identity>>,
  extra: { holdings: Holdings; routes?: Parameters<typeof onboard>[0]['routes'] },
) {
  const done = await onboard({ ...o.onboarding, ...extra })
  const landed = await waitFor(API, done.id, {
    timeoutMs: 60_000,
    client: s.client,
    onboarded: done,
  })
  for (const token of done.tokens) {
    const args = [o.handleNode, token] as const
    expect(await s.client.readContract({ ...s.escrow, functionName: 'escrowed', args })).toBe(0n)
  }
  const session = await s.client.readContract({
    address: o.wallet,
    abi: webWalletAbi,
    functionName: 'sessionOf',
    args: [o.k.address],
  })
  expect(session.admin).toBe(true)
  return { done, landed }
}

test('an onboarding claims the escrowed ETH and repays the relayer from it', async (ctx) => {
  const s = await stack()
  if (!s) {
    if (process.env.CI) throw new Error(`no stack under CI at ${API}`)
    return ctx.skip(`no relayer at ${API}; docker compose up --wait`)
  }
  const o = await identity(s)
  const amount = parseEther('1')
  const args = [o.platform, o.handleNode, s.native, amount] as const
  await s.mined(
    await s.sender.writeContract({ ...s.escrow, functionName: 'deposit', args, value: amount }),
  )

  const holdings = escrowHoldings(s.client, s.escrow.address, [s.native])
  const { done, landed } = await onboarded(s, o, { holdings })
  expect(done.swapped).toEqual([])
  const refund = done.linkFee + done.gasRefund
  expect(landed.received).toEqual([{ token: s.native, amount: amount - refund }])
}, 120_000)

test('an onboarding swaps an approved token for the refund', async (ctx) => {
  const s = await stack()
  if (!s) {
    if (process.env.CI) throw new Error(`no stack under CI at ${API}`)
    return ctx.skip(`no relayer at ${API}; docker compose up --wait`)
  }
  const o = await identity(s)
  // A token at 3000 a coin, and a router holding ETH to pay for it.
  const deploy = async (name: 'MockToken' | 'MockSwapRouter', args: unknown[], value = 0n) => {
    const hash = await s.sender.deployContract({ ...mock(name), args, value })
    return (await s.mined(hash)).contractAddress as Address
  }
  const token = await deploy('MockToken', [])
  const router = await deploy('MockSwapRouter', [3_000_000_000n], parseEther('1'))
  const amount = 10_000_000n
  const erc20 = { address: token, abi: erc20Abi } as const
  const mint = {
    ...erc20,
    abi: mock('MockToken').abi,
    functionName: 'mint',
    args: [s.sender.account.address, amount],
  }
  await s.mined(await s.sender.writeContract(mint))
  await s.mined(
    await s.sender.writeContract({
      ...erc20,
      functionName: 'approve',
      args: [s.escrow.address, amount],
    }),
  )
  const args = [o.platform, o.handleNode, token, amount] as const
  await s.mined(await s.sender.writeContract({ ...s.escrow, functionName: 'deposit', args }))

  const route = { token, fee: 500, router, quoter: router, weth: router }
  const holdings = escrowHoldings(s.client, s.escrow.address, [s.native, token])
  const { done, landed } = await onboarded(s, o, {
    holdings,
    routes: { [s.config.chainId]: [route] },
  })
  // The swap sells exactly the allowance for at least the refund; the surplus
  // ETH stays in the wallet.
  const [swap] = done.swapped
  const refund = done.linkFee + done.gasRefund
  expect(swap.ethOutMin).toBe(refund)
  const allowance = [o.wallet, router] as const
  expect(
    await s.client.readContract({ ...erc20, functionName: 'allowance', args: allowance }),
  ).toBe(0n)
  const [eth, left] = landed.received
  expect(left).toEqual({ token, amount: amount - swap.amountIn })
  expect(eth).toEqual({
    token: s.native,
    amount: (swap.amountIn * 10n ** 18n) / 3_000_000_000n - refund,
  })
}, 120_000)
