// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// A mintable ERC-20 for the e2e test.
contract MockToken {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

/// SwapRouter02's and QuoterV2's functions the client calls, at a fixed
/// `tokensPerEth` and paid from the ETH it holds.
contract MockSwapRouter {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    struct QuoteExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint24 fee;
        uint160 sqrtPriceLimitX96;
    }

    struct QuoteExactOutputSingleParams {
        address tokenIn;
        address tokenOut;
        uint256 amount;
        uint24 fee;
        uint160 sqrtPriceLimitX96;
    }

    uint256 public immutable tokensPerEth;
    uint256 private bought;

    constructor(uint256 tokensPerEth_) payable {
        tokensPerEth = tokensPerEth_;
    }

    receive() external payable {}

    function multicall(uint256 deadline, bytes[] calldata data) external payable returns (bytes[] memory results) {
        require(block.timestamp <= deadline, "deadline");
        results = new bytes[](data.length);
        for (uint256 i; i < data.length; ++i) {
            (bool ok, bytes memory result) = address(this).delegatecall(data[i]);
            require(ok, "call");
            results[i] = result;
        }
    }

    function exactInputSingle(ExactInputSingleParams calldata p) external payable returns (uint256 amountOut) {
        require(p.recipient == address(2), "recipient");
        amountOut = p.amountIn * 1e18 / tokensPerEth;
        require(amountOut >= p.amountOutMinimum, "slippage");
        MockToken(p.tokenIn).transferFrom(msg.sender, address(this), p.amountIn);
        bought += amountOut;
    }

    function unwrapWETH9(uint256 amountMinimum, address recipient) external payable {
        uint256 amount = bought;
        require(amount >= amountMinimum, "minimum");
        bought = 0;
        (bool ok,) = recipient.call{value: amount}("");
        require(ok, "send");
    }

    function quoteExactInputSingle(QuoteExactInputSingleParams calldata p)
        external
        view
        returns (uint256, uint160, uint32, uint256)
    {
        return (p.amountIn * 1e18 / tokensPerEth, 0, 0, 0);
    }

    function quoteExactOutputSingle(QuoteExactOutputSingleParams calldata p)
        external
        view
        returns (uint256, uint160, uint32, uint256)
    {
        return ((p.amount * tokensPerEth + 1e18 - 1) / 1e18, 0, 0, 0);
    }
}
