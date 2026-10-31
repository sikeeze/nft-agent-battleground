// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// Kontrak uji untuk acceptance Fase 5 ( blueprint §9 Fase 5 ).
///
/// Memiliki semua pembaca standar yang dideteksi `nft_mint::sale`:
/// `mint(uint256)`, `price()`, `maxPerWallet()`, `maxSupply()`,
/// `totalSupply()`, `saleActive()`, `balanceOf(address)`, `ownerOf(uint256)`.
///
/// `saleActive` mulai false, lalu `openSale()` mengaktifkannya — dipakai
/// untuk test trigger berbasis state ( §7 ). Pengecekan `saleActive`
/// berlaku hanya saat `qty > 0` supaya probe deteksi `mint(uint256)`
/// ( yang memanggil dengan qty = 0 ) tidak revert saat sale belum dibuka.
contract TestNFT {
    uint256 public constant MAX_SUPPLY = 100;

    /// Fungsi ( bukan constant ) supaya terdeteksi `SaleReader` membaca
    /// `price()` dan `maxPerWallet()`.
    function price() public pure returns (uint256) {
        return 0.001 ether;
    }

    function maxPerWallet() public pure returns (uint256) {
        return 3;
    }

    /// Fungsi ( bukan constant ) — pembaca standar `maxSupply()`.
    function maxSupply() public pure returns (uint256) {
        return MAX_SUPPLY;
    }

    bool public saleActive;
    uint256 public totalSupply;
    mapping(uint256 => address) private _owners;
    mapping(address => uint256) private _balances;

    event Transfer(address indexed from, address indexed to, uint256 indexed tokenId);

    function openSale() external {
        saleActive = true;
    }

    function closeSale() external {
        saleActive = false;
    }

    function mint(uint256 qty) external payable {
        if (qty > 0) {
            require(saleActive, "sale not active");
            require(msg.value == price() * qty, "wrong price");
        }
        require(totalSupply + msg.value / price() <= MAX_SUPPLY, "sold out");
        require(_balances[msg.sender] + msg.value / price() <= maxPerWallet(), "per wallet limit");
        for (uint256 i = 0; i < qty; i++) {
            uint256 id = totalSupply + i;
            _owners[id] = msg.sender;
            _balances[msg.sender] += 1;
            emit Transfer(address(0), msg.sender, id);
        }
        totalSupply += qty;
    }

    function balanceOf(address owner) external view returns (uint256) {
        return _balances[owner];
    }

    function ownerOf(uint256 tokenId) external view returns (address) {
        address owner = _owners[tokenId];
        require(owner != address(0), "nonexistent");
        return owner;
    }
}
