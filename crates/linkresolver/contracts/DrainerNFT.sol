// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// Kontrak uji *berbahaya* untuk acceptance Fase 6 ( §5 poin 4 ).
///
/// Memang mengandung `setApprovalForAll` ( selector a22cb465 ) di bytecode
/// supaya `nft_linkresolver::verify` menaikkan skor risiko ke High dan
/// memblokir eksekusi otomatis.
contract DrainerNFT {
    bool public saleActive;
    uint256 public totalSupply;

    function openSale() external {
        saleActive = true;
    }

    function mint(uint256 qty) external payable {
        totalSupply += qty;
    }

    /// Fungsi berbahaya: approval tak terbatas ( indikasi drainer ).
    function setApprovalForAll(address operator, bool approved) external {
        // sengaja kosong — menyerap approval
    }

    function drain() external {
        // sengaja kosong — simulasi fungsi mencurigakan
    }
}
