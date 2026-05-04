# scripts/

GPU 서버에 OpenAI 호환 Whisper API를 띄우는 가이드.
인프라마다 다르므로 스크립트화하지 않고 명령만 정리.

## 1. NVIDIA Container Toolkit 설치 (CUDA 컨테이너용 1회)

```bash
# 호스트에 docker + nvidia driver 이미 있다고 가정
distribution=$(. /etc/os-release; echo $ID$VERSION_ID)
curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey \
  | sudo gpg --batch --yes --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg
curl -s -L https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list \
  | sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' \
  | sudo tee /etc/apt/sources.list.d/nvidia-container-toolkit.list >/dev/null
sudo apt-get update && sudo apt-get install -y nvidia-container-toolkit
sudo nvidia-ctk runtime configure --runtime=docker
sudo systemctl restart docker
```

> docker 데몬 재시작 시 호스트의 모든 컨테이너가 잠깐 끊김. `restart: unless-stopped`
> 정책의 컨테이너는 자동 복구.

검증:
```bash
docker run --rm --gpus=all nvidia/cuda:12.4.0-base-ubuntu22.04 nvidia-smi
```

## 2. speaches 컨테이너 기동

```bash
docker run -d \
  --name speaches-whisper \
  --gpus=all \
  -p 18001:8000 \
  -e WHISPER__COMPUTE_TYPE=int8_float16 \
  --restart unless-stopped \
  ghcr.io/speaches-ai/speaches:latest-cuda
```

호스트 외부 접근 차단할 거면 `-p 127.0.0.1:18001:8000`로 바꾸고 wrapper는
SSH 터널로 사용.

## 3. large-v3 모델 다운로드 (1회, 약 3GB)

```bash
curl -X POST "http://127.0.0.1:18001/v1/models/Systran/faster-whisper-large-v3"
```

## 4. 헬스체크

```bash
curl http://127.0.0.1:18001/health
curl "http://127.0.0.1:18001/v1/models" | jq .
```

## SSH 터널 (외부 접속 시)

서버를 내부 네트워크에서만 노출하고 wrapper는 사용자 PC에서 SSH 터널로 쓰는 패턴:

```bash
# 사용자 PC에서:
ssh -N -L 18000:127.0.0.1:18001 -p <port> <user>@<server>
# wrapper의 ini는 url=http://127.0.0.1:18000/v1
```

## 보안

- 인증 없이 띄우면 LAN 내 누구나 GPU 사용 가능. 운영망 노출 시 nginx-proxy-manager
  같은 걸로 Bearer 토큰 추가 권장.
- 정 외부에 열어야 하면 wrapper의 `whisper-proxy.ini`에 `api_key = <token>`,
  서버 측에 `--api-key` 또는 인증 프록시 설정.
